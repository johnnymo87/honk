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
    assert!(has_notice(&effs, "stopReason: end_turn"));

    core.handle(Input::Line("hello 2".into()));
    let effs2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": { "stopReason": "cancelled" }
    })));
    assert!(has_notice(&effs2, "stopReason: cancelled"));

    core.handle(Input::Line("hello 3".into()));
    let effs3 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 3,
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
        "id": 4,
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
    };
    let opts_b = ChatOpts {
        crid_seed: 0xcccc_dddd,
        idle_timeout: Some(Duration::from_secs(1800)),
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
    assert!(has_notice(&reply_effs, "steer accepted"));

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

    let conn_effs = core.handle(Input::Connected);
    assert!(has_notice(&conn_effs, "connected"));

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
    assert!(has_notice(&reply_effs, "stopReason: end_turn"));
    assert_eq!(*core.run_state(), RunState::Idle);
}

#[test]
fn test_multiple_lines_while_detached() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x9999,
        idle_timeout: Some(Duration::from_secs(1800)),
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
