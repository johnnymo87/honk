use honk::chat::{
    ChatCore, ChatOpts, Effect, IdAlloc, Input, Notice, NoticeKind, Ownership, RunState,
};
use honk_wire::Cursor;
use serde_json::json;
use std::time::{Duration, Instant};

fn new_test_core() -> (ChatCore, Instant) {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Some(Duration::from_secs(30 * 60)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    // Complete initial replay phase
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": {
            "sessionId": "test-session",
            "epoch": 1,
            "seq": 0
        }
    })));
    (core, now)
}

fn find_notice<'a>(effects: &'a [Effect], sub: &str) -> Option<&'a Notice> {
    effects.iter().find_map(|e| match e {
        Effect::Notice(n) if n.text.contains(sub) => Some(n),
        _ => None,
    })
}

fn has_notice(effects: &[Effect], sub: &str) -> bool {
    find_notice(effects, sub).is_some()
}

#[test]
fn test_ownership_echo_before_run_start_sets_own() {
    let (mut core, _) = new_test_core();
    // 1. Send prompt -> crid is honk-chat-0000000000001234-1
    core.handle(Input::Line("my prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // 2. Real wire order: user_message_chunk echo arrives BEFORE activeRunId!
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "synth_1",
                "content": { "type": "text", "text": "my prompt" },
                "_meta": {
                    "honk": {
                        "source": { "client_req_id": crid }
                    }
                }
            }
        }
    })));

    // 3. session_info_update arrives with activeRunId
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_10" } }
            }
        }
    })));

    assert_eq!(*core.run_state(), RunState::Running("run_10".into()));
    assert_eq!(core.ownership(), Ownership::Own);
}

#[test]
fn test_ownership_foreign_echo_before_run_start_sets_foreign() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("my prompt".into()));

    // Another client's echo arrives before run start
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "foreign_synth",
                "content": { "type": "text", "text": "someone else" },
                "_meta": {
                    "honk": {
                        "source": { "client_req_id": "other-client-crid-99" }
                    }
                }
            }
        }
    })));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign_1" } }
            }
        }
    })));

    assert_eq!(*core.run_state(), RunState::Running("run_foreign_1".into()));
    assert_eq!(core.ownership(), Ownership::Foreign);
}

#[test]
fn test_ownership_no_echo_prompt_in_flight_sets_pending() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("my prompt".into()));

    // Run starts without an echo frame
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_mystery" } }
            }
        }
    })));

    assert_eq!(*core.run_state(), RunState::Running("run_mystery".into()));
    // With our prompt in flight and no echo yet, ownership is Pending!
    assert_eq!(core.ownership(), Ownership::Pending);
}

#[test]
fn test_ownership_late_echo_transitions_pending_to_own() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("my prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Line entered while pending -> held!
    core.handle(Input::Line("held steer".into()));

    // Run starts with no echo -> Pending
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_delayed" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Pending);

    // Late echo arrives inside the run!
    let late_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "late_synth",
                "content": { "type": "text", "text": "my prompt" },
                "_meta": {
                    "honk": {
                        "source": { "client_req_id": crid }
                    }
                }
            }
        }
    })));

    // Transitions to Own and held lines are steered!
    assert_eq!(core.ownership(), Ownership::Own);
    let steer = late_effs.iter().find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer")).unwrap();
    let Effect::Request { params, .. } = steer else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "held steer");
}

#[test]
fn test_parked_behind_foreign_sequence() {
    let (mut core, _) = new_test_core();
    // 1. We send a prompt -> parked in honkd behind another client's turn
    core.handle(Input::Line("our prompt".into()));
    let our_crid = core.prompt_sent().unwrap().crid.clone();

    // 2. Foreign echo + run start arrives
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": "foreign-crid" } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Foreign);

    // 3. User types a line -> held, not steered!
    let hold_effs = core.handle(Input::Line("more work".into()));
    assert!(!hold_effs.iter().any(
        |e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer")
    ));

    // 4. Foreign permission request arrives -> NOT answered, stored as pending
    let perm_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "foreign-perm",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "dangerous_tool" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    })));
    assert!(
        !perm_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(has_notice(&perm_effs, "another client's turn"));

    // 5. Ctrl-C pressed -> REFUSED with notice mentioning /cancel!
    let cancel_effs = core.handle(Input::Interrupt);
    assert!(
        !cancel_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_effs, "/cancel!"));

    // 6. Foreign run ends
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Idle);
    assert_eq!(core.ownership(), Ownership::Unknown);
    // Held line moved to queue because the run ended without becoming Own!
    assert!(has_notice(&end_effs, "held lines moved to queue"));

    // 7. Now honkd releases our parked prompt: our echo arrives, then run start!
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": our_crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "our_run" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("our_run".into()));
    assert_eq!(core.ownership(), Ownership::Own);
}

#[test]
fn test_replay_done_flush_reoffered_permission_unknown_not_answered() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);

    // Initial replay finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 10 }
    })));
    assert_eq!(core.ownership(), Ownership::Unknown);

    // honkd re-offers pending permission right after replay_done
    let reoffer = json!({
        "jsonrpc": "2.0",
        "id": "reoffered-perm-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "unowned_tool" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    });
    let effs = core.handle(Input::Frame(reoffer));
    // Since ownership is Unknown, must NOT auto-answer!
    assert!(!effs.iter().any(|e| matches!(e, Effect::Respond { .. })));
    assert!(has_notice(&effs, "another client's turn"));
}

#[test]
fn test_auto_approve_uses_allow_once() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Echo + run start -> Own
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_approve" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);

    // Permission request with allow_always AND allow_once
    let perm = json!({
        "jsonrpc": "2.0",
        "id": "perm-allow-test",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "shell · cargo build" },
            "options": [
                { "optionId": "opt_always", "name": "always", "kind": "allow_always" },
                { "optionId": "opt_once", "name": "once", "kind": "allow_once" },
                { "optionId": "opt_reject", "name": "reject", "kind": "reject_once" }
            ]
        }
    });

    let effs = core.handle(Input::Frame(perm));
    let resp = effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { result, .. } = resp else {
        panic!()
    };
    // Must choose allow_once over allow_always!
    assert_eq!(result["outcome"]["optionId"], "opt_once");
    assert!(has_notice(&effs, "auto-approved: shell · cargo build"));
}

#[test]
fn test_approve_command_answers_foreign_permission() {
    let (mut core, _) = new_test_core();
    // Foreign turn
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));

    // Request arrives from foreign turn
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "perm-foreign-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "foreign shell" },
            "options": [
                { "optionId": "allow_1", "name": "once", "kind": "allow_once" }
            ]
        }
    })));

    // Human types /approve -> answers the oldest pending request!
    let app_effs = core.handle(Input::Line("/approve".into()));
    let resp = app_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp else {
        panic!()
    };
    assert_eq!(id, "perm-foreign-1");
    assert_eq!(result["outcome"]["optionId"], "allow_1");
    assert!(has_notice(&app_effs, "approved: foreign shell"));

    // /approve again when empty -> warning notice
    let app_effs2 = core.handle(Input::Line("/approve".into()));
    assert!(
        !app_effs2
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(has_notice(&app_effs2, "no pending permission requests"));
}

#[test]
fn test_elicitation_declined_in_own_turn() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "own_run" } }
            }
        }
    })));

    // Elicitation in Own turn
    let elicit = json!({
        "jsonrpc": "2.0",
        "id": "elicit-1",
        "method": "session/elicit_input",
        "params": { "sessionId": "test-session" }
    });
    let effs = core.handle(Input::Frame(elicit));
    let resp = effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp else {
        panic!()
    };
    assert_eq!(id, "elicit-1");
    assert_eq!(result["action"], "decline");
    assert!(has_notice(&effs, "declined elicitation request"));
}

#[test]
fn test_elicitation_not_declined_in_foreign_turn() {
    let (mut core, _) = new_test_core();
    // Foreign turn
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));

    let elicit = json!({
        "jsonrpc": "2.0",
        "id": "elicit-foreign",
        "method": "session/elicit_input",
        "params": { "sessionId": "test-session" }
    });
    let effs = core.handle(Input::Frame(elicit));
    // Must NOT respond!
    assert!(!effs.iter().any(|e| matches!(e, Effect::Respond { .. })));
    assert!(has_notice(
        &effs,
        "elicitation request from another client's turn"
    ));
}

#[test]
fn test_structured_notices_returned_text() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("hello prompt".into()));

    // Lost track error -> NoticeKind::Returned
    let err = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "honkd lost track of this client_req_id"
        }
    });
    let effs = core.handle(Input::Frame(err));
    let notice = find_notice(&effs, "lost track").unwrap();
    assert_eq!(notice.kind, NoticeKind::Returned);
    assert_eq!(notice.returned_text.as_deref(), Some("hello prompt"));
}

#[test]
fn test_attach_success_emits_attached_effect() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);

    let (load_id, _, _) = core.attach_request();

    let load_reply = json!({
        "jsonrpc": "2.0",
        "id": load_id,
        "result": {
            "title": "My Goose Session",
            "_meta": { "extensionResults": [] }
        }
    });

    let effs = core.handle(Input::Frame(load_reply));
    let attached = effs
        .iter()
        .find(|e| matches!(e, Effect::Attached { .. }))
        .unwrap();
    let Effect::Attached { result } = attached else {
        panic!()
    };
    assert_eq!(result["title"], "My Goose Session");
}

#[test]
fn test_attach_failure_emits_attach_failed_effect() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);

    let (load_id, _, _) = core.attach_request();

    let load_err = json!({
        "jsonrpc": "2.0",
        "id": load_id,
        "error": {
            "code": -32000,
            "message": "unheld session requires activate"
        }
    });

    let effs = core.handle(Input::Frame(load_err));
    let failed = effs
        .iter()
        .find(|e| matches!(e, Effect::AttachFailed { .. }))
        .unwrap();
    let Effect::AttachFailed { error } = failed else {
        panic!()
    };
    assert_eq!(error["message"], "unheld session requires activate");
}

#[test]
fn test_getters_queued_len_and_prompt_in_flight() {
    let (mut core, _) = new_test_core();
    assert_eq!(core.queued_len(), 0);
    assert!(!core.prompt_in_flight());

    core.handle(Input::Line("prompt 1".into()));
    assert!(core.prompt_in_flight());
    assert_eq!(core.queued_len(), 0);

    core.handle(Input::Line("held 1".into()));
    assert_eq!(core.queued_len(), 1); // held in held_steers
}

#[test]
fn test_idle_timeout_none_disables_detach() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: None,
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    // 100 hours pass: never detaches!
    let effs = core.handle(Input::Tick(now + Duration::from_secs(100 * 3600)));
    assert!(!core.is_detached());
    assert!(!effs.iter().any(|e| matches!(e, Effect::Detach)));
}

#[test]
fn test_id_alloc_pre_session_constructor() {
    let mut ids = IdAlloc::new();
    let new_session_req_id = ids.allocate();
    assert_eq!(new_session_req_id, 1);

    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 42,
        idle_timeout: None,
        max_turns: None,
    };
    // Pass pre-session allocator to ChatCore::with_ids
    let mut core = ChatCore::with_ids("new-session-id".into(), opts, now, ids);

    let (load_id, method, _) = core.attach_request();
    assert_eq!(load_id, 2); // Monotonic, never collides!
    assert_eq!(method, "session/load");
}

#[test]
fn test_line_idle_unknown_sends_prompt() {
    let (mut core, _) = new_test_core();
    assert_eq!(*core.run_state(), RunState::Unknown);

    let effects = core.handle(Input::Line("hello world".into()));
    let req_effect = effects
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must emit session/prompt request");

    let Effect::Request { id, method, params } = req_effect else {
        panic!()
    };
    assert_eq!(method, "session/prompt");
    assert_eq!(*id, 1);
    assert_eq!(params["prompt"][0]["text"], "hello world");
    assert_eq!(
        params["_meta"]["honk"]["client_req_id"],
        "honk-chat-0000000000001234-1"
    );
    assert!(core.prompt_sent().is_some());
}

#[test]
fn test_line_held_when_prompt_sent_steered_on_run_start() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("initial prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Line while prompt sent but run not started -> held!
    let effs = core.handle(Input::Line("steer while pending".into()));
    assert!(has_notice(&effs, "line held for steer"));

    // Echo + activeRunId arrives
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    let start_effects = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_42" } }
            }
        }
    })));

    assert_eq!(core.ownership(), Ownership::Own);
    let steer_req = start_effects
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer"))
        .expect("must emit steer request for held line");

    let Effect::Request { id, method, params } = steer_req else {
        panic!()
    };
    assert_eq!(method, "_goose/unstable/session/steer");
    assert_eq!(*id, 2);
    assert_eq!(params["expectedRunId"], "run_42");
    assert_eq!(params["prompt"][0]["text"], "steer while pending");
}

#[test]
fn test_line_when_own_run_active_steers() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_99" } }
            }
        }
    })));

    let effects = core.handle(Input::Line("steer into active own run".into()));
    let steer_req = effects
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer"))
        .expect("must emit steer request");

    let Effect::Request { id, method, params } = steer_req else {
        panic!()
    };
    assert_eq!(method, "_goose/unstable/session/steer");
    assert_eq!(*id, 2);
    assert_eq!(params["expectedRunId"], "run_99");
    assert_eq!(params["prompt"][0]["text"], "steer into active own run");
}

#[test]
fn test_line_when_foreign_run_active_queues_and_prompts_after_run_end() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Foreign);

    let eff1 = core.handle(Input::Line("message 1".into()));
    assert!(has_notice(&eff1, "turn belongs to another client"));
    assert_eq!(core.local_queue().len(), 1);

    core.handle(Input::Line("message 2".into()));
    assert_eq!(core.local_queue().len(), 2);

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    let req1 = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must emit session/prompt for message 1");

    let Effect::Request { id, params, .. } = req1 else {
        panic!()
    };
    assert_eq!(*id, 1);
    assert_eq!(params["prompt"][0]["text"], "message 1");
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "message 2");

    let prompt1_reply = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "stopReason": "end_turn" }
    });
    let reply_effs = core.handle(Input::Frame(prompt1_reply));

    let req2 = reply_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must emit session/prompt for message 2");

    let Effect::Request { id, params, .. } = req2 else {
        panic!()
    };
    assert_eq!(*id, 2);
    assert_eq!(params["prompt"][0]["text"], "message 2");
    assert_eq!(core.local_queue().len(), 0);
}

#[test]
fn test_drop_command_clears_queue() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));

    core.handle(Input::Line("msg 1".into()));
    core.handle(Input::Line("msg 2".into()));
    assert_eq!(core.local_queue().len(), 2);

    let drop_effs = core.handle(Input::Line("/drop".into()));
    assert_eq!(core.local_queue().len(), 0);
    assert!(has_notice(&drop_effs, "dropped 2 queued message(s)"));
}

#[test]
fn test_prompt_reply_busy_error_marks_foreign_and_queues() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt while busy".into()));
    assert!(core.prompt_sent().is_some());

    let busy_reply = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32602,
            "message": "Invalid params",
            "data": "session already has active run `run_foreign_busy`; use _goose/unstable/session/steer"
        }
    });
    let effs = core.handle(Input::Frame(busy_reply));

    assert!(core.prompt_sent().is_none());
    assert_eq!(
        *core.run_state(),
        RunState::Running("run_foreign_busy".into())
    );
    assert_eq!(core.ownership(), Ownership::Foreign);
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "prompt while busy");
    assert!(has_notice(&effs, "turn belongs to another client"));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    let prompt_req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must re-prompt when run ended");
    let Effect::Request { id, params, .. } = prompt_req else {
        panic!()
    };
    assert_eq!(*id, 2);
    assert_eq!(
        params["_meta"]["honk"]["client_req_id"],
        "honk-chat-0000000000001234-2"
    );
    assert_eq!(params["prompt"][0]["text"], "prompt while busy");
}

#[test]
fn test_steer_error_no_active_run_converts_once_to_prompt() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer msg".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    let steer_reply = json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "error": {
            "code": -32602,
            "message": "Invalid params",
            "data": "no active run to steer"
        }
    });

    let convert_effs = core.handle(Input::Frame(steer_reply));
    let prompt_req = convert_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must convert steer to prompt");
    let Effect::Request { params, .. } = prompt_req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "steer msg");
    assert!(has_notice(&convert_effs, "converting to prompt"));
}

#[test]
fn test_steer_error_mismatch_prints_text_back() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer mismatch".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    let mismatch_reply = json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "error": {
            "code": -32602,
            "message": "Invalid params",
            "data": {
                "message": "expected active run id `run_1` but found `run_2`",
                "expectedRunId": "run_1",
                "actualRunId": "run_2"
            }
        }
    });

    let reply_effs = core.handle(Input::Frame(mismatch_reply));
    assert!(
        !reply_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { .. }))
    );
    let notice = find_notice(&reply_effs, "the turn you were steering ended").unwrap();
    assert_eq!(notice.kind, NoticeKind::Returned);
    assert_eq!(notice.returned_text.as_deref(), Some("steer mismatch"));
}

#[test]
fn test_at_most_one_conversion_prompt_steer() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let effs = core.handle(Input::Line("msg".into()));
    let steer_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    let effs2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "error": { "code": -32602, "data": "no active run to steer" }
    })));

    let prompt_id = effs2
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "session/prompt" => Some(*id),
            _ => None,
        })
        .unwrap();

    let fail_reply = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "error": { "code": -32602, "message": "cannot run" }
    });
    let final_effs = core.handle(Input::Frame(fail_reply));
    let notice = find_notice(&final_effs, "your message did not run").unwrap();
    assert_eq!(notice.kind, NoticeKind::Returned);
    assert_eq!(notice.returned_text.as_deref(), Some("msg"));
}

#[test]
fn test_unconsumed_steer_reported_at_run_end() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer text unconsumed".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_msg_123" }
    })));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    let notice = find_notice(&end_effs, "steer not consumed").unwrap();
    assert_eq!(notice.kind, NoticeKind::Warning);
    assert!(notice.text.contains("steer text unconsumed"));
}

#[test]
fn test_steer_unconsumed_notification() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer text notify".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_msg_456" }
    })));

    let notif_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/steer_unconsumed",
        "params": { "sessionId": "test-session", "messageId": "steer_msg_456" }
    })));

    let notice = find_notice(&notif_effs, "steer not consumed").unwrap();
    assert_eq!(notice.kind, NoticeKind::Warning);
    assert!(notice.text.contains("steer text notify"));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert!(!has_notice(&end_effs, "steer not consumed"));
}

#[test]
fn test_steer_consumed_via_user_message_chunk() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer text consumed".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_msg_789" }
    })));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "steer_msg_789",
                "content": { "type": "text", "text": "steer text consumed" },
                "_meta": { "goose": { "steer": true, "messageId": "steer_msg_789" } }
            }
        }
    })));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert!(!has_notice(&end_effs, "steer not consumed"));
}

#[test]
fn test_prompt_reply_success_and_cancellation_reasons() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("hello".into()));
    let effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "stopReason": "end_turn" }
    })));
    // end_turn is quiet (no notice)
    assert!(!has_notice(&effs, "stopReason: end_turn"));

    // other stopReason (e.g. max_tokens) emits notice
    core.handle(Input::Line("hello max".into()));
    let effs_max = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": { "stopReason": "max_tokens" }
    })));
    assert!(has_notice(&effs_max, "stopReason: max_tokens"));

    core.handle(Input::Line("hello 2".into()));
    let effs2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": { "stopReason": "cancelled" }
    })));
    assert!(has_notice(&effs2, "stopReason: cancelled"));

    core.handle(Input::Line("hello 3".into()));
    let effs3 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "result": {
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "not_delivered" } }
        }
    })));
    let notice3 = find_notice(&effs3, "your message did not run").unwrap();
    assert_eq!(notice3.kind, NoticeKind::Returned);
    assert_eq!(notice3.returned_text.as_deref(), Some("hello 3"));

    core.handle(Input::Line("hello 4".into()));
    let effs4 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "result": {
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "upstream_restart" } }
        }
    })));
    let notice4 = find_notice(&effs4, "your message did not run").unwrap();
    assert_eq!(notice4.kind, NoticeKind::Returned);
    assert_eq!(notice4.returned_text.as_deref(), Some("hello 4"));
}

#[test]
fn test_prompt_reply_lost_track_terminal() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("lost prompt".into()));
    let effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "honkd lost track of this client_req_id; not resending"
        }
    })));
    assert!(core.prompt_sent().is_none());
    let notice = find_notice(&effs, "lost track").unwrap();
    assert_eq!(notice.kind, NoticeKind::Returned);
    assert_eq!(notice.returned_text.as_deref(), Some("lost prompt"));
}

#[test]
fn test_cancel_own_vs_foreign_vs_force_cancel_vs_idle() {
    let (mut core, _) = new_test_core();

    // Run state still Unknown after attach: refuse, never claim "nothing to cancel".
    let effs = core.handle(Input::Interrupt);
    assert!(!effs.iter().any(|e| matches!(e, Effect::Notify { .. })));
    assert!(has_notice(&effs, "/cancel!"));

    // Known idle: nothing to cancel.
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let effs = core.handle(Input::Interrupt);
    assert!(!effs.iter().any(|e| matches!(e, Effect::Notify { .. })));
    assert!(has_notice(&effs, "no active turn"));

    let effs_cmd = core.handle(Input::Line("/cancel".into()));
    assert!(!effs_cmd.iter().any(|e| matches!(e, Effect::Notify { .. })));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_1" } }
            }
        }
    })));
    let foreign_effs = core.handle(Input::Interrupt);
    assert!(
        !foreign_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { .. }))
    );
    assert!(has_notice(&foreign_effs, "/cancel!"));

    let force_effs = core.handle(Input::Line("/cancel!".into()));
    let notify = force_effs
        .iter()
        .find(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
        .unwrap();
    let Effect::Notify { params, .. } = notify else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "own_1" } }
            }
        }
    })));
    let own_effs = core.handle(Input::Interrupt);
    assert!(
        own_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
}

#[test]
fn test_commands_help_quit_slash_slash() {
    let (mut core, _) = new_test_core();

    let help_effs = core.handle(Input::Line("/help".into()));
    let notice = find_notice(&help_effs, "/quit").unwrap();
    assert!(!notice.text.contains("/rK"));
    assert!(notice.text.contains("/approve"));
    assert!(notice.text.contains("/compact"));

    let unk_effs = core.handle(Input::Line("/foobar".into()));
    assert!(has_notice(&unk_effs, "unknown command: '/foobar'"));
    assert!(!unk_effs.iter().any(|e| matches!(e, Effect::Request { .. })));

    let esc_effs = core.handle(Input::Line("//hello".into()));
    let req = esc_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/hello");

    let quit_effs = core.handle(Input::Line("/quit".into()));
    assert!(
        quit_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/close"))
    );
    assert!(quit_effs.iter().any(|e| matches!(e, Effect::Exit(0))));

    let eof_effs = core.handle(Input::Eof);
    assert!(
        eof_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/close"))
    );
    assert!(eof_effs.iter().any(|e| matches!(e, Effect::Exit(0))));
}

#[test]
fn test_transcript_effects_and_cursor() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    assert_eq!(core.cursor(), None);

    let update_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_1",
                "content": { "type": "text", "text": "Replayed chunk" }
            },
            "_meta": {
                "honk": { "epoch": 1, "seq": 5 }
            }
        }
    });

    let effs = core.handle(Input::Frame(update_frame.clone()));
    let transcript = effs
        .iter()
        .find(|e| matches!(e, Effect::Transcript(_)))
        .unwrap();
    let Effect::Transcript(val) = transcript else {
        panic!()
    };
    assert_eq!(*val, update_frame);
    assert_eq!(core.cursor(), Some(Cursor { epoch: 1, seq: 5 }));

    let replay_done = json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 10 }
    });
    let done_effs = core.handle(Input::Frame(replay_done.clone()));
    assert!(
        done_effs
            .iter()
            .any(|e| matches!(e, Effect::Transcript(v) if *v == replay_done))
    );
    assert_eq!(core.cursor(), Some(Cursor { epoch: 1, seq: 10 }));
}

#[test]
fn test_replay_phase_suppresses_live_actions() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    assert!(core.is_replaying());

    let replayed_perm = json!({
        "jsonrpc": "2.0",
        "id": "replayed-perm-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "replayed tool" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    });
    let effs_perm = core.handle(Input::Frame(replayed_perm));
    assert!(
        !effs_perm
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(!effs_perm.iter().any(|e| matches!(e, Effect::Notice(_))));

    let replayed_gap = json!({
        "jsonrpc": "2.0",
        "method": "_honk/gap",
        "params": { "sessionId": "test-session" }
    });
    let effs_gap = core.handle(Input::Frame(replayed_gap));
    assert!(!effs_gap.iter().any(|e| matches!(e, Effect::Notice(_))));

    let replayed_end = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    });
    let effs_end = core.handle(Input::Frame(replayed_end));
    assert!(!effs_end.iter().any(|e| matches!(e, Effect::Notice(_))));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 1 }
    })));
    assert!(!core.is_replaying());

    // Prompt sent so it's our turn
    core.handle(Input::Line("prompt for live turn".into()));
    let our_crid = core.prompt_sent().unwrap().crid.clone();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": our_crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_live" } }
            }
        }
    })));

    let live_perm = json!({
        "jsonrpc": "2.0",
        "id": "live-perm-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "live tool" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    });
    let effs_live = core.handle(Input::Frame(live_perm));
    assert!(
        effs_live
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
}

#[test]
fn test_attach_request_and_id_allocation() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);

    let (id1, method1, params1) = core.attach_request();
    assert_eq!(id1, 1);
    assert_eq!(method1, "session/load");
    assert_eq!(params1["sessionId"], "test-session");
    assert!(core.is_replaying());

    let (id2, method2, _) = core.attach_request();
    assert_eq!(id2, 2);
    assert_eq!(method2, "session/load");
}

#[test]
fn test_crid_uniqueness_with_seeds() {
    let now = Instant::now();
    let opts_a = ChatOpts {
        crid_seed: 0xaaaa_bbbb,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let opts_b = ChatOpts {
        crid_seed: 0xcccc_dddd,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };

    let mut core_a = ChatCore::new("session-a".into(), opts_a, now);
    let mut core_b = ChatCore::new("session-b".into(), opts_b, now);

    core_a.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "session-a", "epoch": 1, "seq": 0}})));
    core_b.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "session-b", "epoch": 1, "seq": 0}})));

    core_a.handle(Input::Line("hello".into()));
    core_b.handle(Input::Line("hello".into()));

    let crid_a = core_a.prompt_sent().unwrap().crid.clone();
    let crid_b = core_b.prompt_sent().unwrap().crid.clone();

    assert_eq!(crid_a, "honk-chat-00000000aaaabbbb-1");
    assert_eq!(crid_b, "honk-chat-00000000ccccdddd-1");
    assert_ne!(crid_a, crid_b);
}

#[test]
fn test_held_steers_moved_to_queue_when_prompt_clears_without_run() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
    core.handle(Input::Line("held 1".into()));
    core.handle(Input::Line("held 2".into()));

    let fail_reply = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32602, "data": "session already has active run `run_other`; use steer" }
    });
    let effs = core.handle(Input::Frame(fail_reply));
    assert!(has_notice(&effs, "held lines moved to queue"));

    let q: Vec<String> = core.local_queue().iter().map(|(t, _)| t.clone()).collect();
    assert_eq!(q, vec!["prompt 1", "held 1", "held 2"]);
}

#[test]
fn test_line_queued_when_run_seen_but_run_already_idle() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));
    assert!(core.prompt_sent().unwrap().run_seen);

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Idle);

    let effs = core.handle(Input::Line("next line".into()));
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "next line");
    assert!(has_notice(&effs, "queued for next turn"));
}

#[test]
fn test_queue_drains_on_all_prompt_clearing_paths() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": "foreign"}}}}})));
    core.handle(Input::Line("queued 1".into()));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));
    assert_eq!(core.prompt_sent().unwrap().text, "queued 1");

    core.handle(Input::Line("queued 2".into()));
    let effs = core.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}),
    ));
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
    );
    assert_eq!(core.prompt_sent().unwrap().text, "queued 2");

    let (mut core2, _) = new_test_core();
    core2.handle(Input::Line("p1".into()));
    core2.handle(Input::Line("p2".into()));
    let effs2 = core2.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "fail"}}),
    ));
    assert!(
        effs2
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
    );
    assert_eq!(core2.prompt_sent().unwrap().text, "p2");
}

#[test]
fn test_clock_purity_and_deterministic_idle_detach() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    let effs1 = core.handle(Input::Tick(now + Duration::from_secs(1799)));
    assert!(!core.is_detached());
    assert!(!effs1.iter().any(|e| matches!(e, Effect::Detach)));

    let effs2 = core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());
    assert!(effs2.iter().any(|e| matches!(e, Effect::Detach)));
}

#[test]
fn test_detached_lines_buffered_and_processed_in_order_after_replay_done() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());

    let effs1 = core.handle(Input::Line("wake 1".into()));
    assert!(effs1.iter().any(|e| matches!(e, Effect::Reattach)));

    let effs2 = core.handle(Input::Line("wake 2".into()));
    assert!(!effs2.iter().any(|e| matches!(e, Effect::Request { .. })));

    core.attach_request();
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));

    let done_effs = core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 5}})));

    let prompt = done_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "wake 1");
    assert!(core.prompt_sent().is_some());
    let crid = core.prompt_sent().unwrap().crid.clone();
    assert_eq!(core.prompt_sent().unwrap().text, "wake 1");

    // Echo arrives for wake 1
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));

    // When run starts, it is Own and wake 2 is steered!
    let run_start_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_wake" } }
            }
        }
    })));
    let steer = run_start_effs.iter().find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer")).unwrap();
    let Effect::Request { params, .. } = steer else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "wake 2");
}

#[test]
fn test_lost_track_matches_text_not_code_32000_alone() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));

    let err_other = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "held-session limit reached"
        }
    });
    let effs = core.handle(Input::Frame(err_other));
    let notice = find_notice(&effs, "held-session limit reached").unwrap();
    assert_eq!(notice.kind, NoticeKind::Returned);
    assert!(!notice.text.contains("lost track"));
    assert_eq!(notice.returned_text.as_deref(), Some("prompt 1"));
}

#[test]
fn test_steer_race_chunk_before_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer msg".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "steer_early_123",
                "content": { "type": "text", "text": "steer msg" }
            }
        }
    })));

    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_early_123" }
    })));
    // steer accepted notice is dropped (consumed steer appears as user block)
    assert!(!has_notice(&reply_effs, "steer accepted"));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert!(!has_notice(&end_effs, "steer not consumed"));
}

#[test]
fn test_steer_race_reply_after_run_ended() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer msg".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_late_123" }
    })));
    assert!(has_notice(&reply_effs, "steer not consumed"));
}

#[test]
fn test_steer_race_unconsumed_before_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));

    let steer_effs = core.handle(Input::Line("steer msg".into()));
    let steer_id = steer_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_goose/unstable/session/steer" => {
                Some(*id)
            }
            _ => None,
        })
        .unwrap();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/steer_unconsumed",
        "params": { "sessionId": "test-session", "messageId": "steer_pre_unconsumed" }
    })));

    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_pre_unconsumed" }
    })));
    assert!(has_notice(&reply_effs, "steer not consumed"));
}

#[test]
fn test_disconnected_resets_run_state_and_ownership_to_unknown() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "own_1" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("own_1".into()));
    assert_eq!(core.ownership(), Ownership::Own);

    core.handle(Input::Disconnected);
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);
    assert!(core.prompt_sent().is_some());
    assert!(core.is_replaying());
}

#[test]
fn test_reconnect_resends_prompt_with_same_crid() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt before disconnect".into()));
    let prompt_sent = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent.crid, "honk-chat-0000000000001234-1");
    assert_eq!(prompt_sent.req_id, 1);

    core.handle(Input::Line("steer before disconnect".into()));

    let disc_effs = core.handle(Input::Disconnected);
    assert!(has_notice(&disc_effs, "disconnected; reconnecting..."));
    // Subsequent disconnect during same outage does not duplicate notice
    let disc_effs2 = core.handle(Input::Disconnected);
    assert!(!has_notice(&disc_effs2, "disconnected; reconnecting..."));

    let conn_effs = core.handle(Input::Connected);
    // connected notice dropped
    assert!(!has_notice(&conn_effs, "connected"));

    let resend_req = conn_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { id, params, .. } = resend_req else {
        panic!()
    };
    assert_eq!(*id, 2);
    assert_eq!(
        params["_meta"]["honk"]["client_req_id"],
        "honk-chat-0000000000001234-1"
    );
    assert_eq!(params["prompt"][0]["text"], "prompt before disconnect");

    let prompt_sent_after = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent_after.crid, "honk-chat-0000000000001234-1");
    assert_eq!(prompt_sent_after.req_id, 2);
}

#[test]
fn test_evicted_and_gap_frames() {
    let (mut core, _) = new_test_core();

    let gap_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/gap",
        "params": { "sessionId": "test-session" }
    })));
    assert!(has_notice(&gap_effs, "frame gap detected"));

    let evicted_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/evicted",
        "params": { "sessionId": "test-session" }
    })));
    assert!(evicted_effs.iter().any(|e| matches!(e, Effect::Reattach)));
    assert!(has_notice(
        &evicted_effs,
        "session evicted; re-attaching..."
    ));
}

#[test]
fn test_run_end_before_prompt_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("p1".into()));
    core.handle(Input::Line("held_or_queued".into()));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_early_end" } }
            }
        }
    })));

    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Idle);
    assert!(
        !end_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
    );

    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "stopReason": "end_turn" }
    })));
    // end_turn is quiet
    assert!(!has_notice(&reply_effs, "stopReason: end_turn"));
    assert_eq!(*core.run_state(), RunState::Idle);
}

#[test]
fn test_multiple_lines_while_detached() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x9999,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());

    core.handle(Input::Line("line 1".into()));
    core.handle(Input::Line("line 2".into()));
    core.handle(Input::Line("line 3".into()));

    let done_effs = core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 0}})));

    let prompt = done_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "line 1");
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Echo arrives for line 1
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));

    let run_start_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_multi" } }
            }
        }
    })));

    let steers: Vec<String> = run_start_effs
        .iter()
        .filter_map(|e| match e {
            Effect::Request { method, params, .. } if method == "_goose/unstable/session/steer" => {
                Some(params["prompt"][0]["text"].as_str().unwrap().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(steers, vec!["line 2", "line 3"]);
}

#[test]
fn test_disconnect_with_held_steers() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
    core.handle(Input::Line("held steer".into()));

    let disc_effs = core.handle(Input::Disconnected);
    assert!(has_notice(&disc_effs, "disconnected; reconnecting..."));

    core.handle(Input::Connected);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 0}})));

    let fail_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "error": { "code": -32600, "message": "fail" }
    })));
    let prompt2 = fail_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt2 else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "held steer");
}

#[test]
fn test_prompt_error_with_non_empty_queue() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_1" } }
            }
        }
    })));
    core.handle(Input::Line("item 1".into()));
    core.handle(Input::Line("item 2".into()));
    core.handle(Input::Line("item 3".into()));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert_eq!(core.prompt_sent().unwrap().text, "item 1");
    assert_eq!(core.local_queue().len(), 2);

    let err_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32000, "message": "temporary upstream failure" }
    })));
    assert!(has_notice(&err_effs, "temporary upstream failure"));
    assert!(has_notice(&err_effs, "your message did not run: item 1"));

    let next_prompt = err_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = next_prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "item 2");
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "item 3");
}

#[test]
fn test_replayed_active_run_id_and_request_permission_do_not_trigger_live_behaviour() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: None,
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    assert!(core.is_replaying());

    let r1 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "replayed_run" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("replayed_run".into()));
    assert!(!r1.iter().any(|e| matches!(
        e,
        Effect::Notice(_) | Effect::Request { .. } | Effect::Notify { .. }
    )));

    let r2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Idle);
    assert!(!r2.iter().any(|e| matches!(
        e,
        Effect::Notice(_) | Effect::Request { .. } | Effect::Notify { .. }
    )));

    let r3 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "replayed-perm",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "hist_tool" },
            "options": [{ "optionId": "o", "name": "o", "kind": "allow_always" }]
        }
    })));
    assert!(
        !r3.iter()
            .any(|e| matches!(e, Effect::Respond { .. } | Effect::Notice(_)))
    );
}

#[test]
fn test_review_scenario_a_reconnect_replay_echo_does_not_grant_own_to_foreign_run() {
    let (mut core, _) = new_test_core();
    // 1. Prompt crid-1 completes
    core.handle(Input::Line("prompt 1".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "stopReason": "end_turn" }
    })));
    assert_eq!(*core.run_state(), RunState::Idle);

    // 2. Disconnect + reconnect
    core.handle(Input::Disconnected);
    core.handle(Input::Connected);

    // Replay includes crid-1 echo
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));

    // Replay done
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 2, "seq": 5 }
    })));

    // 3. Foreign run starts (no echo)
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign_x" } }
            }
        }
    })));

    // MUST be Foreign!
    assert_eq!(core.ownership(), Ownership::Foreign);

    // Its permission request is NOT answered
    let perm_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "foreign-perm-req",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "foreign shell" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    })));
    assert!(
        !perm_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(has_notice(&perm_effs, "another client's turn"));

    // Ctrl-C is refused
    let cancel_effs = core.handle(Input::Interrupt);
    assert!(
        !cancel_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_effs, "/cancel!"));
}

#[test]
fn test_review_scenario_b_foreign_run_active_old_crid_echo_stays_foreign() {
    let (mut core, _) = new_test_core();
    // Prompt 1
    core.handle(Input::Line("prompt 1".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();
    // Complete prompt 1
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "user_message_chunk", "_meta": {"honk": {"source": {"client_req_id": crid1}}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": "run_1"}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));
    core.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}),
    ));

    // Foreign run starts
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign_2" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Foreign);

    // Held line
    core.handle(Input::Line("steer attempt".into()));

    // An echo with an old crid of ours arrives
    let effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));

    // Must STAY Foreign! No steer request emitted!
    assert_eq!(core.ownership(), Ownership::Foreign);
    assert!(!effs.iter().any(
        |e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer")
    ));
}

#[test]
fn test_review_scenario_c_reset_mid_session_replays_no_live_effects() {
    let (mut core, _) = new_test_core();

    // Reset arrives mid-session
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/reset",
        "params": { "sessionId": "test-session", "epoch": 2 }
    })));
    assert!(core.is_replaying());

    // During this replay phase, replayed activeRunId and permission must have no live effects
    let r1 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_replayed" } }
            }
        }
    })));
    assert!(
        !r1.iter()
            .any(|e| matches!(e, Effect::Notice(_) | Effect::Request { .. }))
    );

    let r2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "perm-replayed",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "replayed tool" },
            "options": [{ "optionId": "o1", "name": "o1", "kind": "allow_once" }]
        }
    })));
    assert!(
        !r2.iter()
            .any(|e| matches!(e, Effect::Respond { .. } | Effect::Notice(_)))
    );
}

#[test]
fn test_review_scenario_d_held_lines_with_run_seen_true_and_error_reply_queued() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();

    // Echo + run start -> run_seen is true
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));
    assert!(core.prompt_sent().unwrap().run_seen);

    // Run ends before prompt reply
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    // User types line while prompt is still in flight: queued
    core.handle(Input::Line("follow up line".into()));

    // Prompt reply fails with an error
    let effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32600, "message": "unexpected failure" }
    })));

    // The queued line must be dispatched as prompt now!
    let next_prompt = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = next_prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "follow up line");
}

#[test]
fn test_review_scenario_e_approve_then_reoffer_answered_same_way_no_double() {
    let (mut core, _) = new_test_core();
    // Foreign turn
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));

    let perm_frame = json!({
        "jsonrpc": "2.0",
        "id": "perm-id-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "bash command" },
            "options": [
                { "optionId": "allow_once_opt", "name": "once", "kind": "allow_once" }
            ]
        }
    });
    core.handle(Input::Frame(perm_frame.clone()));

    // Human approves with /approve
    let app_effs = core.handle(Input::Line("/approve".into()));
    let resp1 = app_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp1 else {
        panic!()
    };
    assert_eq!(id, "perm-id-1");
    assert_eq!(result["outcome"]["optionId"], "allow_once_opt");

    // Server re-offers the same request (e.g. upstream failed and honkd re-offers)
    let reoffer_effs = core.handle(Input::Frame(perm_frame));
    // Automatically re-answered identically!
    let resp2 = reoffer_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond {
        id: id2,
        result: result2,
    } = resp2
    else {
        panic!()
    };
    assert_eq!(id2, "perm-id-1");
    assert_eq!(result2["outcome"]["optionId"], "allow_once_opt");
}

#[test]
fn test_review_scenario_f_unknown_prompt_in_flight_ctrl_c_refused() {
    let (mut core, _) = new_test_core();
    // Prompt in flight while run_state is Unknown (no run announced yet)
    core.handle(Input::Line("unstarted prompt".into()));
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);
    assert!(core.prompt_sent().is_some());

    // Ctrl-C must be REFUSED because ownership is not Own!
    let cancel_effs = core.handle(Input::Interrupt);
    assert!(
        !cancel_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_effs, "/cancel!"));
}

#[test]
fn test_reset_clears_run_state_and_ownership_and_refuses_cancel() {
    let (mut core, _) = new_test_core();
    // 1. Send prompt
    core.handle(Input::Line("prompt 1".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();
    // Echo arrives
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));
    // Active run announced -> Own
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);
    assert_eq!(*core.run_state(), RunState::Running("run_1".into()));

    // 2. _honk/reset arrives mid-run
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/reset",
        "params": { "sessionId": "test-session", "epoch": 2 }
    })));
    // Reset MUST set run_state = Unknown and run_ownership = Unknown, and is_replaying = true
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);
    assert!(core.is_replaying());

    // While replaying, Ctrl-C must be refused!
    let cancel_during_replay = core.handle(Input::Interrupt);
    assert!(
        !cancel_during_replay
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_during_replay, "/cancel!"));

    // Replay frames with NO activeRunId frames
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_chunk_1",
                "content": { "type": "text", "text": "replayed text" }
            }
        }
    })));

    // replay_done arrives
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 2, "seq": 5 }
    })));
    assert!(!core.is_replaying());
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);

    // After replay_done, Ctrl-C must still be refused because ownership is Unknown!
    let cancel_after_replay = core.handle(Input::Interrupt);
    assert!(
        !cancel_after_replay
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_after_replay, "/cancel!"));

    // Line typed while prompt 1 is still in flight: queued in local_queue
    core.handle(Input::Line("second line".into()));
    assert_eq!(core.local_queue().len(), 1);

    // Prompt reply arrives (stopReason cancelled, reason upstream_restart)
    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "upstream_restart" } }
        }
    })));

    // Queued lines drain!
    let next_prompt = reply_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = next_prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "second line");
}

#[test]
fn test_reconnect_reclaims_own_turn_when_replay_contains_echo() {
    let (mut core, _) = new_test_core();
    // 1. Send prompt -> crid1
    core.handle(Input::Line("gated prompt".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();

    // 2. Disconnect mid-turn
    core.handle(Input::Disconnected);
    assert_eq!(core.ownership(), Ownership::Unknown);

    // 3. Connect (reconnect) -> prompt re-sent with crid1
    core.handle(Input::Connected);

    // 4. Replay arrives: contains our echo + activeRunId
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "gated_run_1" } }
            }
        }
    })));

    // 5. replay_done arrives
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 2, "seq": 10 }
    })));

    // Ownership must be RECLAIMED as Own!
    assert_eq!(core.ownership(), Ownership::Own);
    assert_eq!(*core.run_state(), RunState::Running("gated_run_1".into()));

    // Live permission arrives -> auto-approved!
    let perm_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "perm-gated",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "gated tool" },
            "options": [{ "optionId": "allow_once", "name": "once", "kind": "allow_once" }]
        }
    })));
    assert!(
        perm_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(has_notice(&perm_effs, "auto-approved: gated tool"));

    // Ctrl-C arrives -> cancels!
    let cancel_effs = core.handle(Input::Interrupt);
    assert!(
        cancel_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_effs, "cancelling turn..."));
}

#[test]
fn test_reconnect_foreign_run_with_old_crid_stays_non_own() {
    let (mut core, _) = new_test_core();
    // 1. Prompt 0 completes
    core.handle(Input::Line("prompt 0".into()));
    let crid0 = core.prompt_sent().unwrap().crid.clone();
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "user_message_chunk", "_meta": {"honk": {"source": {"client_req_id": crid0}}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": "run_0"}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));
    core.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}),
    ));

    // 2. New prompt 1 sent
    core.handle(Input::Line("prompt 1".into()));
    let crid1 = core.prompt_sent().unwrap().crid.clone();
    assert_ne!(crid0, crid1);

    // 3. Disconnect mid-turn
    core.handle(Input::Disconnected);

    // 4. Reconnect
    core.handle(Input::Connected);

    // Replay contains old crid0 echo, then null, then foreign run start
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "user_message_chunk", "_meta": {"honk": {"source": {"client_req_id": crid0}}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": "foreign_run_xyz"}}}}})));

    // replay_done
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 20}})));

    // Must NOT be Own!
    assert_ne!(core.ownership(), Ownership::Own);

    // Permission request must NOT be auto-approved
    let perm_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "foreign-perm",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "foreign tool" },
            "options": [{ "optionId": "o", "name": "o", "kind": "allow_once" }]
        }
    })));
    assert!(
        !perm_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(has_notice(&perm_effs, "another client's turn"));

    // Ctrl-C must be refused
    let cancel_effs = core.handle(Input::Interrupt);
    assert!(
        !cancel_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(has_notice(&cancel_effs, "/cancel!"));
}

#[test]
fn test_prompt_params_carry_max_turns_when_set() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: Some(5),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 0 }
    })));

    let effs = core.handle(Input::Line("hello".into()));
    let prompt = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert_eq!(params["_meta"]["goose"]["maxTurns"], 5);
    assert!(params["_meta"]["honk"]["client_req_id"].is_string());
}

#[test]
fn test_prompt_params_omit_max_turns_when_unset() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("hello".into()));
    let prompt = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert!(params["_meta"]["goose"].is_null());
    assert!(params["_meta"]["honk"]["client_req_id"].is_string());
}

#[test]
fn test_prompt_re_send_keeps_max_turns() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: Some(3),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 0 }
    })));

    core.handle(Input::Line("hello".into()));
    core.handle(Input::Disconnected);
    let reconnected_effs = core.handle(Input::Connected);

    let resend = reconnected_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = resend else {
        panic!()
    };
    assert_eq!(params["_meta"]["goose"]["maxTurns"], 3);
}

#[test]
fn test_turns_slash_command_updates_subsequent_prompts() {
    let (mut core, _) = new_test_core();

    // /turns 7 updates max_turns
    let effs1 = core.handle(Input::Line("/turns 7".into()));
    assert!(has_notice(&effs1, "max_turns set to 7"));

    let effs_p1 = core.handle(Input::Line("prompt 1".into()));
    let p1 = effs_p1
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request {
        params: params1, ..
    } = p1
    else {
        panic!()
    };
    assert_eq!(params1["_meta"]["goose"]["maxTurns"], 7);

    // Turn finishes
    let req_id1 = core.prompt_sent().unwrap().req_id;
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": req_id1,
        "result": { "stopReason": "end_turn" }
    })));
    let crid1 = "honk-chat-0000000000001234-1";
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u1",
                "content": { "type": "text", "text": "prompt 1" },
                "_meta": { "honk": { "source": { "client_req_id": crid1 } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "update": { "sessionUpdate": "session_info_update", "_meta": { "goose": { "activeRunId": null } } } }
    })));

    // /turns off clears it
    let effs2 = core.handle(Input::Line("/turns off".into()));
    assert!(has_notice(&effs2, "max_turns disabled"));

    let effs_p2 = core.handle(Input::Line("prompt 2".into()));
    let p2 = effs_p2
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request {
        params: params2, ..
    } = p2
    else {
        panic!()
    };
    assert!(params2["_meta"]["goose"].is_null());
}

#[test]
fn test_turns_slash_command_invalid_argument() {
    let (mut core, _) = new_test_core();

    let effs0 = core.handle(Input::Line("/turns 0".into()));
    assert!(has_notice(&effs0, "invalid /turns argument"));
    assert_eq!(core.max_turns(), None);

    let effs_abc = core.handle(Input::Line("/turns abc".into()));
    assert!(has_notice(&effs_abc, "invalid /turns argument"));
    assert_eq!(core.max_turns(), None);
}

#[test]
fn test_steer_does_not_carry_max_turns() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: Some(10),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 0 }
    })));

    // Prompt 1
    core.handle(Input::Line("prompt 1".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Echo confirms own turn
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u1",
                "content": { "type": "text", "text": "prompt 1" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "update": { "sessionUpdate": "session_info_update", "_meta": { "goose": { "activeRunId": "run_1" } } } }
    })));

    // While own turn is running, line becomes a steer
    let steer_effs = core.handle(Input::Line("steer text".into()));
    let steer = steer_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer"))
        .unwrap();
    let Effect::Request { params, .. } = steer else {
        panic!()
    };
    assert!(params.get("_meta").is_none());
}

#[test]
fn test_compact_idle_sends_prompt() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/compact".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send session/prompt request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
    assert!(core.prompt_sent().is_some());
    assert_eq!(core.prompt_sent().unwrap().text, "/compact");
}

#[test]
fn test_compact_idle_preserves_max_turns() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Some(Duration::from_secs(1800)),
        max_turns: Some(5),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 0 }
    })));

    let effs = core.handle(Input::Line("/compact".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send session/prompt request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
    assert_eq!(params["_meta"]["goose"]["maxTurns"], 5);
}

#[test]
fn test_compact_with_args_rejected() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/compact now".into()));
    assert!(has_notice(&effs, "/compact takes no arguments"));
    assert!(core.prompt_sent().is_none());
    assert_eq!(core.queued_len(), 0);
    assert!(!effs.iter().any(|e| matches!(e, Effect::Request { .. })));
}

#[test]
fn test_compact_queued_during_own_run_no_steer() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("run something".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();

    // Confirm own run
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u1",
                "content": { "type": "text", "text": "run something" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_own" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);

    // Issue /compact while own run is active
    let effs = core.handle(Input::Line("/compact".into()));
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "should not emit steer request for /compact"
    );
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt")),
        "should not emit prompt request immediately"
    );
    assert!(has_notice(
        &effs,
        "compaction queued; it runs after the current turn ends"
    ));
    assert_eq!(core.queued_len(), 1);
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "/compact");
    assert!(core.held_steers().is_empty());

    // Run ends upstream and prompt response returns
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let prompt_req_id = core.prompt_sent().unwrap().req_id;
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    // After turn ends, prompt for /compact is sent!
    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send /compact prompt after own run ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
    assert_eq!(core.queued_len(), 0);
    assert!(core.local_queue().is_empty());
    assert!(core.held_steers().is_empty());
}

#[test]
fn test_compact_queued_while_prompt_sent_before_run_starts() {
    let (mut core, _) = new_test_core();
    // 1. Send normal prompt "initial prompt"
    core.handle(Input::Line("initial prompt".into()));
    assert!(core.prompt_sent().is_some());
    assert!(!core.run_state().is_running());
    let crid = core.prompt_sent().unwrap().crid.clone();
    let prompt_req_id = core.prompt_sent().unwrap().req_id;

    // 2. Type /compact while prompt is sent but run has not started
    let effs = core.handle(Input::Line("/compact".into()));
    assert!(has_notice(
        &effs,
        "compaction queued; it runs after the current turn ends"
    ));
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "should not emit steer request for /compact"
    );
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt")),
        "should not emit prompt request immediately"
    );
    // Exact placement: goes to local_queue, never held_steers
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "/compact");
    assert!(core.held_steers().is_empty());

    // 3. Now the initial prompt's turn starts (echo + activeRunId)
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u_init",
                "content": { "type": "text", "text": "initial prompt" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    let run_start_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_initial" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);
    // Crucial check: /compact was NOT held for steer and was NOT steered on run start
    assert!(
        !run_start_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "/compact must never be steered into run"
    );
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "/compact");
    assert!(core.held_steers().is_empty());

    // 4. Initial turn finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    // 5. /compact goes out as a prompt only after that run ends
    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send /compact prompt after initial run ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
    assert!(core.local_queue().is_empty());
    assert!(core.held_steers().is_empty());
}

#[test]
fn test_compact_queued_behind_another_queued_line() {
    let (mut core, _) = new_test_core();
    // Foreign run starts
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run_1" } }
            }
        }
    })));

    // Queue regular message "a"
    core.handle(Input::Line("a".into()));
    // Queue /compact
    let effs = core.handle(Input::Line("/compact".into()));
    assert!(has_notice(
        &effs,
        "compaction queued; it runs after the current turn ends"
    ));
    assert_eq!(core.queued_len(), 2);

    // Foreign run ends
    let drain_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    // "a" should be sent as prompt, NOT /compact
    let req = drain_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send prompt for 'a'");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "a");
    assert_eq!(core.queued_len(), 1);

    // "a" starts its own run
    let crid_a = core.prompt_sent().unwrap().crid.clone();
    let prompt_req_id = core.prompt_sent().unwrap().req_id;
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "ua",
                "content": { "type": "text", "text": "a" },
                "_meta": { "honk": { "source": { "client_req_id": crid_a } } }
            }
        }
    })));
    let run_start_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_a" } }
            }
        }
    })));
    // Assert no steer emitted for /compact
    assert!(
        !run_start_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "queued /compact must NOT be steered into run 'a'"
    );

    // "a" finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    // Now /compact is sent as prompt!
    let req2 = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send /compact prompt after 'a' ends");
    let Effect::Request { params: p2, .. } = req2 else {
        panic!()
    };
    assert_eq!(p2["prompt"][0]["text"], "/compact");
    assert_eq!(core.queued_len(), 0);
}

#[test]
fn test_drop_command_clears_compact() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "foreign_run" } }
            }
        }
    })));

    core.handle(Input::Line("msg 1".into()));
    core.handle(Input::Line("/compact".into()));
    assert_eq!(core.queued_len(), 2);

    let drop_effs = core.handle(Input::Line("/drop".into()));
    assert_eq!(core.queued_len(), 0);
    assert!(has_notice(&drop_effs, "dropped 2 queued message(s)"));

    // When foreign run ends, nothing is sent
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    assert!(!end_effs.iter().any(|e| matches!(e, Effect::Request { .. })));
}

#[test]
fn test_compact_buffered_while_detached() {
    let (mut core, _) = new_test_core();
    // Simulate idle detach
    core.handle(Input::Tick(Instant::now() + Duration::from_secs(3600)));
    assert!(core.is_detached());

    let effs = core.handle(Input::Line("/compact".into()));
    assert!(effs.iter().any(|e| matches!(e, Effect::Reattach)));
    assert_eq!(core.queued_len(), 1);

    // Replay finishes
    let replay_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 2, "seq": 0 }
    })));

    let req = replay_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send /compact prompt after replay done");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
}

#[test]
fn test_compact_inflight_own_running_queues_line_no_steer() {
    let (mut core, _) = new_test_core();
    // 1. Send /compact prompt
    core.handle(Input::Line("/compact".into()));
    assert!(core.prompt_sent().is_some());
    let crid = core.prompt_sent().unwrap().crid.clone();
    let prompt_req_id = core.prompt_sent().unwrap().req_id;

    // 2. /compact run starts (echo + activeRunId)
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u_compact",
                "content": { "type": "text", "text": "/compact" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_compact" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("run_compact".into()));
    assert_eq!(core.ownership(), Ownership::Own);

    // 3. User types a line while /compact is running (Own+Running)
    let effs = core.handle(Input::Line("message during compaction".into()));
    // Must NOT steer!
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "lines typed during compaction must not steer"
    );
    assert!(has_notice(
        &effs,
        "compaction running; message queued for after it ends"
    ));
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "message during compaction");
    assert!(core.held_steers().is_empty());

    // 4. Compaction finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    // 5. Queued message is sent as prompt
    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send queued message prompt after compaction ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "message during compaction");
    assert!(core.local_queue().is_empty());
}

#[test]
fn test_compact_inflight_line_typed_before_run_announced_queues_no_held_steer() {
    let (mut core, _) = new_test_core();
    // 1. Send /compact prompt
    core.handle(Input::Line("/compact".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();
    let prompt_req_id = core.prompt_sent().unwrap().req_id;

    // 2. Before run is announced, type a line
    let effs = core.handle(Input::Line("typed before compaction starts".into()));
    assert!(has_notice(
        &effs,
        "compaction running; message queued for after it ends"
    ));
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "typed before compaction starts");
    assert!(core.held_steers().is_empty());

    // 3. /compact run announced
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u_compact",
                "content": { "type": "text", "text": "/compact" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    let run_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_compact" } }
            }
        }
    })));
    // Must NOT emit steer
    assert!(
        !run_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "no steer flushed into compaction run"
    );
    assert_eq!(core.local_queue().len(), 1);
    assert!(core.held_steers().is_empty());

    // 4. Compaction finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send prompt after compaction ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(
        params["prompt"][0]["text"],
        "typed before compaction starts"
    );
}

#[test]
fn test_compact_buffered_reattach_with_subsequent_line_queues_no_steer() {
    let (mut core, _) = new_test_core();
    // Detach via idle
    core.handle(Input::Tick(Instant::now() + Duration::from_secs(3600)));
    assert!(core.is_detached());

    // Buffer /compact and then next line while detached
    core.handle(Input::Line("/compact".into()));
    core.handle(Input::Line("next line".into()));
    assert_eq!(core.queued_len(), 2);

    // Replay finishes
    let replay_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 2, "seq": 0 }
    })));

    // /compact was sent as prompt
    let req = replay_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send /compact prompt after replay done");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/compact");
    let crid = core.prompt_sent().unwrap().crid.clone();
    let prompt_req_id = core.prompt_sent().unwrap().req_id;

    // "next line" must be in local_queue, NOT held_steers
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "next line");
    assert!(core.held_steers().is_empty());

    // Compaction run starts
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "u_c",
                "content": { "type": "text", "text": "/compact" },
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    let run_start_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_c" } }
            }
        }
    })));
    // Must NOT steer "next line"
    assert!(
        !run_start_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method.contains("steer"))),
        "buffered next line must NOT be steered into compaction run"
    );

    // Compaction finishes
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    let req2 = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("should send next line prompt after compaction ends");
    let Effect::Request { params: p2, .. } = req2 else {
        panic!()
    };
    assert_eq!(p2["prompt"][0]["text"], "next line");
}

#[test]
fn test_help_command_includes_mcp() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/help".into()));
    let notice = effs
        .iter()
        .find_map(|e| match e {
            Effect::Notice(n) => Some(&n.text),
            _ => None,
        })
        .unwrap();
    assert!(notice.contains("/mcp"));
}

#[test]
fn test_mcp_list_idle_sends_request() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/list"))
        .expect("should send _honk/mcp/list request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");
}

#[test]
fn test_mcp_list_during_active_run_sends_request_immediately() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("run_foreign".into()));

    let effs = core.handle(Input::Line("/mcp".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/list"))
        .expect("list should run immediately even during active run");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");
}

#[test]
fn test_mcp_usage_missing_names() {
    let (mut core, _) = new_test_core();

    let effs = core.handle(Input::Line("/mcp on".into()));
    assert!(has_notice(&effs, "usage: /mcp on <name>"));

    let effs = core.handle(Input::Line("/mcp on!".into()));
    assert!(has_notice(&effs, "usage: /mcp on! <name>"));

    let effs = core.handle(Input::Line("/mcp off".into()));
    assert!(has_notice(&effs, "usage: /mcp off <name>"));

    let effs = core.handle(Input::Line("/mcp invalid".into()));
    assert!(has_notice(&effs, "unknown /mcp subcommand"));
}

#[test]
fn test_mcp_on_idle_sends_enable() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp on slack-ro".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/enable"))
        .expect("should send _honk/mcp/enable request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");
    assert_eq!(params["names"], json!(["slack-ro"]));
    assert_eq!(params["confirm"], false);

    let effs = core.handle(Input::Line("/mcp on a b".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/enable"))
        .expect("should send _honk/mcp/enable request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["names"], json!(["a", "b"]));
    assert_eq!(params["confirm"], false);
}

#[test]
fn test_mcp_on_confirm_idle_sends_enable() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp on! slack".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/enable"))
        .expect("should send _honk/mcp/enable request with confirm");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");
    assert_eq!(params["names"], json!(["slack"]));
    assert_eq!(params["confirm"], true);
}

#[test]
fn test_mcp_off_idle_sends_disable() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp off slack-ro".into()));
    let req = effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/disable"))
        .expect("should send _honk/mcp/disable request");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");
    assert_eq!(params["names"], json!(["slack-ro"]));
}

#[test]
fn test_mcp_on_queued_during_own_run() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("run something".into()));
    let prompt_req_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "session/prompt" => Some(*id),
            _ => None,
        })
        .unwrap();

    let crid = core.prompt_sent().unwrap().crid.clone();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "_meta": { "honk": { "source": { "client_req_id": crid } } }
            }
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_1" } }
            }
        }
    })));
    assert_eq!(*core.run_state(), RunState::Running("run_1".into()));

    // Queue /mcp on slack-ro
    let effs = core.handle(Input::Line("/mcp on slack-ro".into()));
    assert!(has_notice(&effs, "queued: /mcp on slack-ro"));
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/enable"))
    );
    assert_eq!(core.local_queue()[0].0, "/mcp on slack-ro");

    // End run
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": prompt_req_id,
        "result": { "stopReason": "end_turn" }
    })));

    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/enable"))
        .expect("should send queued _honk/mcp/enable request after turn ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["names"], json!(["slack-ro"]));
    assert_eq!(params["confirm"], false);
}

#[test]
fn test_mcp_off_queued_during_foreign_run() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign" } }
            }
        }
    })));

    let effs = core.handle(Input::Line("/mcp off slack-ro".into()));
    assert!(has_notice(&effs, "queued: /mcp off slack-ro"));
    assert_eq!(core.local_queue()[0].0, "/mcp off slack-ro");

    // Turn ends
    let end_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": null } }
            }
        }
    })));

    let req = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/disable"))
        .expect("should send queued _honk/mcp/disable request after turn ends");
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["names"], json!(["slack-ro"]));
}

#[test]
fn test_drop_command_clears_queued_mcp() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_foreign" } }
            }
        }
    })));

    core.handle(Input::Line("/mcp on slack-ro".into()));
    assert_eq!(core.local_queue().len(), 1);

    let effs = core.handle(Input::Line("/drop".into()));
    assert!(has_notice(&effs, "dropped 1 queued message(s)"));
    assert_eq!(core.local_queue().len(), 0);
}

#[test]
fn test_mcp_list_response_rendering() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp".into()));
    let req_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_honk/mcp/list" => Some(*id),
            _ => None,
        })
        .unwrap();

    let resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "servers": [
                { "name": "slack", "enabled": false, "supported": true, "confirm": true },
                { "name": "slack-ro", "enabled": true, "supported": true, "confirm": false },
                { "name": "other-conflict", "enabled": false, "supported": true, "confirm": false, "conflict": true },
                { "name": "datadog", "enabled": false, "supported": false, "reason": "remote MCP servers not supported yet", "confirm": false }
            ],
            "other": [
                { "key": "developer" },
                { "key": "analyze" }
            ]
        }
    });

    let resp_effs = core.handle(Input::Frame(resp));
    let notice = resp_effs
        .iter()
        .find_map(|e| match e {
            Effect::Notice(n) => Some(&n.text),
            _ => None,
        })
        .expect("should emit notice with list block");

    assert!(notice.contains("off slack !"));
    assert!(notice.contains("on slack-ro"));
    assert!(notice.contains("off other-conflict (conflict)"));
    assert!(notice.contains("n/a datadog — remote MCP servers not supported yet"));
    assert!(notice.contains("other: developer, analyze"));
    assert!(
        notice.contains("/mcp on <name> · /mcp on! <name> for servers marked ! · /mcp off <name>")
    );
    assert_eq!(core.mcp_enabled(), &["slack-ro"]);
}

#[test]
fn test_mcp_enable_response_rendering() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp on slack-ro".into()));
    let req_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_honk/mcp/enable" => Some(*id),
            _ => None,
        })
        .unwrap();

    let resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "results": [
                { "name": "slack-ro", "ok": true, "changed": true },
                { "name": "slack", "ok": true, "replaced": "slack-ro", "changed": true },
                { "name": "broken", "ok": false, "error": "failed to start", "changed": false }
            ],
            "servers": [
                { "name": "slack", "enabled": true, "supported": true, "confirm": true }
            ],
            "other": []
        }
    });

    let resp_effs = core.handle(Input::Frame(resp));
    assert!(has_notice(&resp_effs, "mcp: slack-ro on"));
    assert!(has_notice(&resp_effs, "mcp: slack on (replaced slack-ro)"));
    assert!(has_notice(
        &resp_effs,
        "mcp: broken failed: failed to start"
    ));
    assert_eq!(core.mcp_enabled(), &["slack"]);
}

#[test]
fn test_mcp_disable_response_rendering() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp off slack".into()));
    let req_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_honk/mcp/disable" => Some(*id),
            _ => None,
        })
        .unwrap();

    let resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "results": [
                { "name": "slack", "ok": true, "changed": true },
                { "name": "already-off", "ok": true, "note": "already off", "changed": false },
                { "name": "failed-srv", "ok": false, "error": "permission denied", "changed": false }
            ],
            "servers": [],
            "other": []
        }
    });

    let resp_effs = core.handle(Input::Frame(resp));
    assert!(has_notice(&resp_effs, "mcp: slack off"));
    assert!(has_notice(&resp_effs, "mcp: already-off off (already off)"));
    assert!(has_notice(
        &resp_effs,
        "mcp: failed-srv failed: permission denied"
    ));
    assert_eq!(core.mcp_enabled(), &[] as &[String]);
}

#[test]
fn test_mcp_error_verbatim_notice() {
    let (mut core, _) = new_test_core();
    let effs = core.handle(Input::Line("/mcp on slack".into()));
    let req_id = effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_honk/mcp/enable" => Some(*id),
            _ => None,
        })
        .unwrap();

    let err_msg =
        "'slack' can write; use /mcp on! slack to confirm (or /mcp on slack-ro for read-only)";
    let resp = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {
            "code": -32602,
            "message": err_msg
        }
    });

    let resp_effs = core.handle(Input::Frame(resp));
    let notice = resp_effs
        .iter()
        .find_map(|e| match e {
            Effect::Notice(n) if n.text.contains(err_msg) => Some(n),
            _ => None,
        })
        .expect("should emit verbatim error notice");
    assert_eq!(notice.kind, NoticeKind::Error);
}

#[test]
fn test_mcp_catalog_unconfigured_stops_auto_refresh() {
    let (mut core, _) = new_test_core();

    // Initial attach or background refresh
    let req = core
        .auto_refresh_mcp()
        .expect("should trigger auto-refresh");
    let Effect::Request { id: req_id, .. } = req else {
        panic!()
    };

    // Honkd responds with -32601 (not configured)
    let resp_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {
            "code": -32601,
            "message": "MCP catalog not configured (honkd --mcp-catalog)"
        }
    })));
    // Must NOT spam notice on auto-refresh error
    assert!(!has_notice(&resp_effs, "MCP catalog not configured"));

    // Future auto-refresh returns None (stops auto-refreshing)
    assert!(core.auto_refresh_mcp().is_none());

    // Explicit /mcp still sends request and shows error notice
    let explicit_effs = core.handle(Input::Line("/mcp".into()));
    let explicit_req_id = explicit_effs
        .iter()
        .find_map(|e| match e {
            Effect::Request { id, method, .. } if method == "_honk/mcp/list" => Some(*id),
            _ => None,
        })
        .expect("explicit /mcp should still send request");

    let explicit_resp_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": explicit_req_id,
        "error": {
            "code": -32601,
            "message": "MCP catalog not configured (honkd --mcp-catalog)"
        }
    })));
    assert!(has_notice(
        &explicit_resp_effs,
        "MCP catalog not configured (honkd --mcp-catalog)"
    ));
}

#[test]
fn test_mcp_changed_notification() {
    let (mut core, _) = new_test_core();

    // From other client with change summary
    let effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/mcp_changed",
        "params": {
            "sessionId": "test-session",
            "enabled": ["slack-ro", "atlassian"],
            "change": "enabled slack-ro",
            "origin": "other"
        }
    })));
    assert!(has_notice(
        &effs,
        "mcp: enabled slack-ro (by another client)"
    ));
    assert_eq!(core.mcp_enabled(), &["atlassian", "slack-ro"]);

    // From you: no extra notice
    let effs2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/mcp_changed",
        "params": {
            "sessionId": "test-session",
            "enabled": ["atlassian"],
            "change": "disabled slack-ro",
            "origin": "you"
        }
    })));
    assert!(!has_notice(&effs2, "another client"));
    assert_eq!(core.mcp_enabled(), &["atlassian"]);

    // Raw change without enabled array triggers re-list
    let effs3 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/mcp_changed",
        "params": {
            "sessionId": "test-session",
            "origin": "other"
        }
    })));
    assert!(
        effs3
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "_honk/mcp/list"))
    );
}
