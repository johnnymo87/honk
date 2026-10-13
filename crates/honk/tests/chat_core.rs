use honk::chat::{ChatCore, ChatOpts, Effect, Input, Ownership, RunState};
use honk_wire::Cursor;
use serde_json::json;
use std::time::{Duration, Instant};

fn new_test_core() -> (ChatCore, Instant) {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x1234,
        idle_timeout: Duration::from_secs(30 * 60),
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

#[test]
fn test_run_state_transitions_and_initial_unknown() {
    let (mut core, _) = new_test_core();
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);

    // Frame session_info_update with activeRunId string
    let run_start_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {
                    "goose": {
                        "activeRunId": "run_123"
                    }
                }
            }
        }
    });
    core.handle(Input::Frame(run_start_frame));
    assert_eq!(*core.run_state(), RunState::Running("run_123".into()));
    assert_eq!(core.ownership(), Ownership::Foreign);

    // Frame session_info_update with activeRunId null
    let run_end_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {
                    "goose": {
                        "activeRunId": null
                    }
                }
            }
        }
    });
    core.handle(Input::Frame(run_end_frame));
    assert_eq!(*core.run_state(), RunState::Idle);
    assert_eq!(core.ownership(), Ownership::Unknown);
}

#[test]
fn test_line_idle_unknown_sends_prompt() {
    let (mut core, _) = new_test_core();
    assert_eq!(*core.run_state(), RunState::Unknown);

    let effects = core.handle(Input::Line("hello world".into()));
    assert_eq!(effects.len(), 2);

    let req_effect = effects
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .expect("must emit session/prompt request");

    let Effect::Request { id, method, params } = req_effect else {
        panic!();
    };
    assert_eq!(method, "session/prompt");
    assert_eq!(*id, 1);
    assert_eq!(params["sessionId"], "test-session");
    assert_eq!(params["prompt"][0]["text"], "hello world");
    let crid = params["_meta"]["honk"]["client_req_id"]
        .as_str()
        .expect("must have client_req_id");
    assert_eq!(crid, "honk-chat-0000000000001234-1");

    assert!(core.prompt_sent().is_some());
    let prompt_sent = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent.crid, "honk-chat-0000000000001234-1");
    assert_eq!(prompt_sent.text, "hello world");
    assert_eq!(prompt_sent.req_id, 1);
    assert!(!prompt_sent.run_seen);
}

#[test]
fn test_line_held_when_prompt_sent_steered_on_run_start() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("initial prompt".into()));
    assert!(core.prompt_sent().is_some());
    assert!(!core.prompt_sent().unwrap().run_seen);

    let effects = core.handle(Input::Line("steer while pending".into()));
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::Status(msg) => assert!(msg.contains("line held for steer")),
        other => panic!("expected status message, got {other:?}"),
    }

    let run_start_frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {
                    "goose": {
                        "activeRunId": "run_42"
                    }
                }
            }
        }
    });
    let start_effects = core.handle(Input::Frame(run_start_frame));

    assert_eq!(core.ownership(), Ownership::Own);
    assert!(core.prompt_sent().unwrap().run_seen);

    let steer_req = start_effects
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "_goose/unstable/session/steer"))
        .expect("must emit steer request for held line");

    let Effect::Request { id, method, params } = steer_req else {
        panic!();
    };
    assert_eq!(method, "_goose/unstable/session/steer");
    assert_eq!(*id, 2);
    assert_eq!(params["sessionId"], "test-session");
    assert_eq!(params["expectedRunId"], "run_42");
    assert_eq!(params["prompt"][0]["text"], "steer while pending");
}

#[test]
fn test_line_when_own_run_active_steers() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
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
        panic!();
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
    assert_eq!(eff1.len(), 1);
    match &eff1[0] {
        Effect::Status(msg) => {
            assert!(msg.contains("turn belongs to another client"));
            assert!(msg.contains("/drop"));
        }
        other => panic!("expected status, got {other:?}"),
    }
    assert_eq!(core.local_queue().len(), 1);

    let eff2 = core.handle(Input::Line("message 2".into()));
    assert_eq!(eff2.len(), 1);
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
        "result": {
            "stopReason": "end_turn"
        }
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
    assert_eq!(drop_effs.len(), 1);
    match &drop_effs[0] {
        Effect::Status(msg) => assert!(msg.contains("dropped 2 queued message(s)")),
        other => panic!("expected status, got {other:?}"),
    }
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

    let status = effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("/drop")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("turn belongs to another client"));

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
    assert!(
        convert_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("converting")))
    );
}

#[test]
fn test_steer_error_mismatch_prints_text_back() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt 1".into()));
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
    let status = reply_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("the turn you were steering ended")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("steer mismatch"));
}

#[test]
fn test_at_most_one_conversion_prompt_steer() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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

    let _ = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_2" } }
            }
        }
    })));

    let fail_reply = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "error": { "code": -32602, "message": "cannot run" }
    });
    let final_effs = core.handle(Input::Frame(fail_reply));
    let status = final_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("your message did not run")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("msg"));
}

#[test]
fn test_unconsumed_steer_reported_at_run_end() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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
        "result": {
            "runId": "run_1",
            "messageId": "steer_msg_123"
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

    let status = end_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("steer text unconsumed"));
}

#[test]
fn test_steer_unconsumed_notification() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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
        "result": {
            "runId": "run_1",
            "messageId": "steer_msg_456"
        }
    })));

    let notif_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/steer_unconsumed",
        "params": {
            "sessionId": "test-session",
            "messageId": "steer_msg_456"
        }
    })));

    let status = notif_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("steer text notify"));

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
    assert!(
        !end_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
    );
}

#[test]
fn test_steer_consumed_via_user_message_chunk() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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
        "result": {
            "runId": "run_1",
            "messageId": "steer_msg_789"
        }
    })));

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "steer_msg_789",
                "content": { "type": "text", "text": "steer text consumed" },
                "_meta": {
                    "goose": {
                        "steer": true,
                        "messageId": "steer_msg_789"
                    }
                }
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
    assert!(
        !end_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
    );
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
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("stopReason: end_turn")))
    );

    core.handle(Input::Line("hello 2".into()));
    let effs2 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": { "stopReason": "cancelled" }
    })));
    assert!(
        effs2
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("stopReason: cancelled")))
    );

    core.handle(Input::Line("hello 3".into()));
    let effs3 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "not_delivered" } }
        }
    })));
    let status3 = effs3
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("your message did not run")))
        .unwrap();
    let Effect::Status(msg3) = status3 else {
        panic!()
    };
    assert!(msg3.contains("hello 3"));

    core.handle(Input::Line("hello 4".into()));
    let effs4 = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "result": {
            "stopReason": "cancelled",
            "_meta": { "honk": { "reason": "upstream_restart" } }
        }
    })));
    let status4 = effs4
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("your message did not run")))
        .unwrap();
    let Effect::Status(msg4) = status4 else {
        panic!()
    };
    assert!(msg4.contains("hello 4"));
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
    let status = effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("lost track")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("lost prompt"));
}

#[test]
fn test_ownership_prompt_sent_vs_echo_crid() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt A".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();
    assert_eq!(crid, "honk-chat-0000000000001234-1");

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_a" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);

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
    assert_eq!(core.ownership(), Ownership::Unknown);

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": { "goose": { "activeRunId": "run_b" } }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Foreign);

    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": "msg_b",
                "content": { "type": "text", "text": "prompt A" },
                "_meta": {
                    "honk": {
                        "source": {
                            "client_req_id": "honk-chat-0000000000001234-1"
                        }
                    }
                }
            }
        }
    })));
    assert_eq!(core.ownership(), Ownership::Own);
}

#[test]
fn test_cancel_own_vs_foreign_vs_force_cancel_vs_idle() {
    let (mut core, _) = new_test_core();

    let effs = core.handle(Input::Interrupt);
    assert!(!effs.iter().any(|e| matches!(e, Effect::Notify { .. })));
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("no active turn")))
    );

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
    assert!(
        foreign_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("/cancel!")))
    );

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
fn test_cancel_foreign_run_with_prompt_in_flight_refuses() {
    let (mut core, _) = new_test_core();
    // Foreign run starts
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

    // Prompt sent while foreign run is active (queued)
    core.handle(Input::Line("line while foreign".into()));

    // Now user hits /cancel -> MUST refuse because active run is foreign!
    let effs = core.handle(Input::Line("/cancel".into()));
    assert!(
        !effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("/cancel!")))
    );
}

#[test]
fn test_cancel_no_run_with_prompt_in_flight_cancels() {
    let (mut core, _) = new_test_core();
    // Prompt sent while idle/unknown
    core.handle(Input::Line("prompt before run".into()));
    assert!(core.prompt_sent().is_some());
    assert_eq!(*core.run_state(), RunState::Unknown);

    // Cancel now targets our in-flight prompt!
    let effs = core.handle(Input::Line("/cancel".into()));
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
    );
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("cancelling turn")))
    );
}

#[test]
fn test_auto_approve_permissions_own_turn() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
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

    // Permission request during own turn
    let perm = json!({
        "jsonrpc": "2.0",
        "id": "req-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "shell · cargo test" },
            "options": [
                { "optionId": "allow_once", "name": "allow_once", "kind": "allow_once" },
                { "optionId": "allow_always", "name": "allow_always", "kind": "allow_always" },
                { "optionId": "reject_once", "name": "reject_once", "kind": "reject_once" }
            ]
        }
    });

    let effs = core.handle(Input::Frame(perm));
    let resp = effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp else {
        panic!()
    };
    assert_eq!(id, "req-1");
    // "allow_always" preferred over "allow_once"
    assert_eq!(result["outcome"]["outcome"], "selected");
    assert_eq!(result["outcome"]["optionId"], "allow_always");

    let status = effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("auto-approved")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("shell · cargo test"));
}

#[test]
fn test_auto_approve_permissions_foreign_turn_refusal() {
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

    let perm = json!({
        "jsonrpc": "2.0",
        "id": "req-foreign",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "secret_tool" },
            "options": [{ "optionId": "allow", "name": "allow", "kind": "allow" }]
        }
    });

    let effs = core.handle(Input::Frame(perm));
    // Must NOT answer!
    assert!(!effs.iter().any(|e| matches!(e, Effect::Respond { .. })));
    let status = effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("another client's turn")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("secret_tool"));
}

#[test]
fn test_permission_dedup_reoffer() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
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

    let perm = json!({
        "jsonrpc": "2.0",
        "id": "req-reoffer",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "bash" },
            "options": [{ "optionId": "opt1", "name": "opt1", "kind": "allow_once" }]
        }
    });

    let effs1 = core.handle(Input::Frame(perm.clone()));
    assert!(effs1.iter().any(|e| matches!(e, Effect::Respond { .. })));

    // Re-offered: answered again identically
    let effs2 = core.handle(Input::Frame(perm));
    let resp2 = effs2
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp2 else {
        panic!()
    };
    assert_eq!(id, "req-reoffer");
    assert_eq!(result["outcome"]["optionId"], "opt1");
}

#[test]
fn test_commands_help_quit_slash_slash() {
    let (mut core, _) = new_test_core();

    let help_effs = core.handle(Input::Line("/help".into()));
    let status = help_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(_)))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("/quit"));
    assert!(!msg.contains("/rK")); // Permission commands removed

    let unk_effs = core.handle(Input::Line("/foobar".into()));
    assert!(
        unk_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("unknown command: '/foobar'")))
    );
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
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);

    assert_eq!(core.cursor(), None);

    // Frame session/update during replay
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
    // Emits Effect::Transcript, NOT Effect::Print
    let transcript = effs
        .iter()
        .find(|e| matches!(e, Effect::Transcript(_)))
        .unwrap();
    let Effect::Transcript(val) = transcript else {
        panic!()
    };
    assert_eq!(*val, update_frame);
    assert_eq!(core.cursor(), Some(Cursor { epoch: 1, seq: 5 }));

    // _honk/replay_done
    let replay_done = json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": {
            "sessionId": "test-session",
            "epoch": 1,
            "seq": 10
        }
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
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    assert!(core.is_replaying());

    // 1. Replayed permission request -> NOT answered, NO status
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
    assert!(!effs_perm.iter().any(|e| matches!(e, Effect::Status(_))));

    // 2. Replayed _honk/gap -> NO status
    let replayed_gap = json!({
        "jsonrpc": "2.0",
        "method": "_honk/gap",
        "params": { "sessionId": "test-session" }
    });
    let effs_gap = core.handle(Input::Frame(replayed_gap));
    assert!(!effs_gap.iter().any(|e| matches!(e, Effect::Status(_))));

    // 3. Replayed activeRunId: null -> NO unconsumed steer reports, NO status
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
    assert!(!effs_end.iter().any(|e| matches!(e, Effect::Status(_))));

    // Now replay_done arrives: replay phase ends!
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": { "sessionId": "test-session", "epoch": 1, "seq": 1 }
    })));
    assert!(!core.is_replaying());

    // Send a prompt so it is our turn
    core.handle(Input::Line("prompt for live turn".into()));

    // Live permission arrives -> now answered!
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
        idle_timeout: Duration::from_secs(1800),
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
        idle_timeout: Duration::from_secs(1800),
    };
    let opts_b = ChatOpts {
        crid_seed: 0xcccc_dddd,
        idle_timeout: Duration::from_secs(1800),
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
    // Hold a line
    core.handle(Input::Line("held 1".into()));
    core.handle(Input::Line("held 2".into()));

    // Prompt reply fails with busy error before run was announced
    let fail_reply = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32602, "data": "session already has active run `run_other`; use steer" }
    });
    let effs = core.handle(Input::Frame(fail_reply));

    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("held lines moved to queue")))
    );
    // Prompt text and held lines are at front of queue:
    let q: Vec<String> = core.local_queue().iter().map(|(t, _)| t.clone()).collect();
    assert_eq!(q, vec!["prompt 1", "held 1", "held 2"]);
}

#[test]
fn test_line_queued_when_run_seen_but_run_already_idle() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
    // Announce run
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

    // Run ends before prompt reply arrives
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

    // New line entered while run_seen is true but run_state is Idle: MUST go to queue, not held!
    let effs = core.handle(Input::Line("next line".into()));
    assert_eq!(core.local_queue().len(), 1);
    assert_eq!(core.local_queue()[0].0, "next line");
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("queued for next turn")))
    );
}

#[test]
fn test_queue_drains_on_all_prompt_clearing_paths() {
    // 1. Success path
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": "foreign"}}}}})));
    core.handle(Input::Line("queued 1".into()));
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));
    // queued 1 is sent as prompt (id: 1)
    assert_eq!(core.prompt_sent().unwrap().text, "queued 1");
    // Add another queued line
    core.handle(Input::Line("queued 2".into()));
    // Prompt reply arrives with success
    let effs = core.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"stopReason": "end_turn"}}),
    ));
    // Must dispatch queued 2!
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
    );
    assert_eq!(core.prompt_sent().unwrap().text, "queued 2");

    // 2. Error path
    let (mut core2, _) = new_test_core();
    core2.handle(Input::Line("p1".into()));
    core2.handle(Input::Line("p2".into())); // held
    // p1 fails with generic error
    let effs2 = core2.handle(Input::Frame(
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "fail"}}),
    ));
    // Drains queue!
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
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    // 1799 seconds pass: not detached
    let effs1 = core.handle(Input::Tick(now + Duration::from_secs(1799)));
    assert!(!core.is_detached());
    assert!(!effs1.iter().any(|e| matches!(e, Effect::Detach)));

    // 1800 seconds pass: detached!
    let effs2 = core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());
    assert!(effs2.iter().any(|e| matches!(e, Effect::Detach)));
}

#[test]
fn test_detached_lines_buffered_and_processed_in_order_after_replay_done() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 1,
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    // Detach
    core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());

    // Enter line 1 while detached -> triggers Reattach, buffers line 1
    let effs1 = core.handle(Input::Line("wake 1".into()));
    assert!(effs1.iter().any(|e| matches!(e, Effect::Reattach)));

    // Enter line 2 while re-attaching -> buffers line 2
    let effs2 = core.handle(Input::Line("wake 2".into()));
    assert!(!effs2.iter().any(|e| matches!(e, Effect::Request { .. })));

    // Reattach load happens
    core.attach_request();

    // Replay frames arrive
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "session_info_update", "_meta": {"goose": {"activeRunId": null}}}}})));

    // Replay ends!
    let done_effs = core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 5}})));

    // wake 1 is dispatched as prompt, wake 2 is held for steer once run starts!
    let prompt = done_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "wake 1");
    assert!(core.prompt_sent().is_some());
    assert_eq!(core.prompt_sent().unwrap().text, "wake 1");

    // When run starts, wake 2 is steered into it!
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

    // Code -32000 with a different message (e.g. held session limit reached)
    let err_other = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "held-session limit reached"
        }
    });
    let effs = core.handle(Input::Frame(err_other));
    let status = effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("held-session limit reached")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(!msg.contains("lost track"));
    assert!(msg.contains("your message did not run: prompt 1"));
}

#[test]
fn test_steer_race_chunk_before_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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

    // user_message_chunk arrives BEFORE the steer reply!
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

    // Now steer reply arrives:
    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_early_123" }
    })));
    assert!(
        reply_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer accepted")))
    );

    // When run ends later, it must NOT be reported unconsumed!
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
    assert!(
        !end_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
    );
}

#[test]
fn test_steer_race_reply_after_run_ended() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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

    // Run ends BEFORE steer reply arrives!
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

    // Steer reply arrives after run ended: MUST report unconsumed!
    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_late_123" }
    })));
    assert!(
        reply_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
    );
}

#[test]
fn test_steer_race_unconsumed_before_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("init".into()));
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

    // _honk/steer_unconsumed arrives BEFORE steer reply!
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/steer_unconsumed",
        "params": { "sessionId": "test-session", "messageId": "steer_pre_unconsumed" }
    })));

    // Steer reply arrives:
    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": { "runId": "run_1", "messageId": "steer_pre_unconsumed" }
    })));
    assert!(
        reply_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("steer not consumed")))
    );
}

#[test]
fn test_disconnected_resets_run_state_and_ownership_to_unknown() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt".into()));
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

    // Disconnect
    core.handle(Input::Disconnected);
    assert_eq!(*core.run_state(), RunState::Unknown);
    assert_eq!(core.ownership(), Ownership::Unknown);
    assert!(core.prompt_sent().is_some()); // Prompt retained for resend
    assert!(core.is_replaying());
}

#[test]
fn test_reconnect_resends_prompt_with_same_crid() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("prompt before disconnect".into()));
    let prompt_sent = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent.crid, "honk-chat-0000000000001234-1");
    assert_eq!(prompt_sent.req_id, 1);

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
    core.handle(Input::Line("steer before disconnect".into()));

    let disc_effs = core.handle(Input::Disconnected);
    assert!(
        disc_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("disconnected; reconnecting...")))
    );
    assert!(disc_effs.iter().any(|e| matches!(e, Effect::Status(m) if m.contains("steer may not have been delivered: steer before disconnect"))));

    let conn_effs = core.handle(Input::Connected);
    assert!(
        conn_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("connected")))
    );

    let resend_req = conn_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { id, params, .. } = resend_req else {
        panic!()
    };
    assert_eq!(*id, 3);
    assert_eq!(
        params["_meta"]["honk"]["client_req_id"],
        "honk-chat-0000000000001234-1"
    );
    assert_eq!(params["prompt"][0]["text"], "prompt before disconnect");

    let prompt_sent_after = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent_after.crid, "honk-chat-0000000000001234-1");
    assert_eq!(prompt_sent_after.req_id, 3);
}

#[test]
fn test_evicted_and_gap_frames() {
    let (mut core, _) = new_test_core();

    let gap_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/gap",
        "params": { "sessionId": "test-session" }
    })));
    assert!(
        gap_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("frame gap detected")))
    );

    let evicted_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/evicted",
        "params": { "sessionId": "test-session" }
    })));
    assert!(evicted_effs.iter().any(|e| matches!(e, Effect::Reattach)));
    assert!(
        evicted_effs.iter().any(
            |e| matches!(e, Effect::Status(m) if m.contains("session evicted; re-attaching..."))
        )
    );
}

#[test]
fn test_run_end_before_prompt_reply() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Line("p1".into()));
    // Queue p2 while p1 is in flight
    core.handle(Input::Line("held_or_queued".into())); // held for steer

    // Run starts
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

    // Run ends BEFORE prompt reply arrives
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
    // Queue should not drain yet because prompt 1 is still in flight!
    assert!(
        !end_effs
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
    );

    // Prompt reply arrives
    let reply_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "stopReason": "end_turn" }
    })));
    assert!(
        reply_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("stopReason: end_turn")))
    );
    // Since run was already idle, prompt reply completes and state remains idle
    assert_eq!(*core.run_state(), RunState::Idle);
}

#[test]
fn test_multiple_lines_while_detached() {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_seed: 0x9999,
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 1, "seq": 0}})));

    // Detach via idle Tick
    core.handle(Input::Tick(now + Duration::from_secs(1800)));
    assert!(core.is_detached());

    // Enter line 1, 2, 3 while detached
    core.handle(Input::Line("line 1".into()));
    core.handle(Input::Line("line 2".into()));
    core.handle(Input::Line("line 3".into()));

    // Replay done after reattach
    let done_effs = core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 0}})));

    // Line 1 sent as prompt
    let prompt = done_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "line 1");

    // When run starts, line 2 and line 3 are steered into it in order!
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

    // Disconnect happens before run announced
    let disc_effs = core.handle(Input::Disconnected);
    assert!(
        disc_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("disconnected; reconnecting...")))
    );

    // Reconnect arrives
    core.handle(Input::Connected);
    core.handle(Input::Frame(json!({"jsonrpc": "2.0", "method": "_honk/replay_done", "params": {"sessionId": "test-session", "epoch": 2, "seq": 0}})));

    // If prompt 1 fails, held steer must be moved to queue
    let fail_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 2, // New req id from reconnect resend
        "error": { "code": -32600, "message": "fail" }
    })));
    // Drains queue by dispatching held steer as prompt!
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
    // Start with a foreign run so we can queue messages
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

    // Foreign run ends -> item 1 dispatched as prompt (id: 1)
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

    // Item 1 fails with an error -> item 2 must be dispatched promptly!
    let err_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32000, "message": "temporary upstream failure" }
    })));
    assert!(
        err_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("temporary upstream failure")))
    );
    assert!(
        err_effs.iter().any(
            |e| matches!(e, Effect::Status(m) if m.contains("your message did not run: item 1"))
        )
    );

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
        idle_timeout: Duration::from_secs(1800),
    };
    let mut core = ChatCore::new("test-session".into(), opts, now);
    assert!(core.is_replaying());

    // 1. Replayed activeRunId start and null
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
        Effect::Status(_) | Effect::Request { .. } | Effect::Notify { .. }
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
        Effect::Status(_) | Effect::Request { .. } | Effect::Notify { .. }
    )));

    // 2. Replayed request_permission
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
            .any(|e| matches!(e, Effect::Respond { .. } | Effect::Status(_)))
    );
}
