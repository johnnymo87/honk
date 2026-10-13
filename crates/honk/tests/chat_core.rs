use honk::chat::{ChatCore, ChatOpts, Effect, Input, Ownership, RunState};
use honk_wire::Cursor;
use serde_json::json;
use std::time::Instant;

fn new_test_core() -> (ChatCore, Instant) {
    let now = Instant::now();
    let opts = ChatOpts {
        crid_prefix: Some("test".into()),
        ..Default::default()
    };
    (ChatCore::new("test-session".into(), opts, now), now)
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
    // Since we did not have a prompt in flight, this run is foreign
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

    // Input Line when Unknown acts like Idle: sends session/prompt
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
    assert_eq!(crid, "test-1");

    assert!(core.prompt_sent().is_some());
    let prompt_sent = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent.crid, "test-1");
    assert_eq!(prompt_sent.text, "hello world");
    assert_eq!(prompt_sent.req_id, 1);
    assert!(!prompt_sent.run_seen);
}

#[test]
fn test_line_held_when_prompt_sent_steered_on_run_start() {
    let (mut core, _) = new_test_core();
    // 1. Send initial prompt
    core.handle(Input::Line("initial prompt".into()));
    assert!(core.prompt_sent().is_some());
    assert!(!core.prompt_sent().unwrap().run_seen);

    // 2. Line while prompt sent but run not started -> held!
    let effects = core.handle(Input::Line("steer while pending".into()));
    assert_eq!(effects.len(), 1);
    match &effects[0] {
        Effect::Status(msg) => assert!(msg.contains("line held for steer")),
        other => panic!("expected status message, got {other:?}"),
    }

    // 3. activeRunId arrives
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

    // Must have steered the held line into run_42!
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
    // Send prompt and announce run
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

    // Line while OWN run is active
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

    // Line 1: queued locally
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

    // Line 2: also queued
    let eff2 = core.handle(Input::Line("message 2".into()));
    assert_eq!(eff2.len(), 1);
    assert_eq!(core.local_queue().len(), 2);

    // Run ends (activeRunId goes null)
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

    // Message 1 must be sent as a prompt! Message 2 stays in queue
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

    // Prompt 1 reply arrives (turn finished)
    let prompt1_reply = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "stopReason": "end_turn"
        }
    });
    let reply_effs = core.handle(Input::Frame(prompt1_reply));

    // Message 2 must now be sent as a prompt!
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
    // Send prompt while unknown
    core.handle(Input::Line("prompt while busy".into()));
    assert!(core.prompt_sent().is_some());

    // Reply is busy error
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

    // Prompt slot is cleared
    assert!(core.prompt_sent().is_none());
    // Run is marked running foreign
    assert_eq!(
        *core.run_state(),
        RunState::Running("run_foreign_busy".into())
    );
    assert_eq!(core.ownership(), Ownership::Foreign);
    // Text is queued at front of local queue
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

    // When the foreign run ends, queued text is sent with a fresh crid!
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
    // Fresh crid (test-2, not test-1)
    assert_eq!(params["_meta"]["honk"]["client_req_id"], "test-2");
    assert_eq!(params["prompt"][0]["text"], "prompt while busy");
}

#[test]
fn test_steer_error_no_active_run_converts_once_to_prompt() {
    let (mut core, _) = new_test_core();
    // Simulate own run
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

    // Send steer
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

    // Steer reply: no active run
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

    // Must convert to session/prompt
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

    // Must print text back and not retarget/prompt
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
    // Start own run
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

    // Steer line
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

    // First conversion: steer -> prompt
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

    // Now if that prompt or steer fails again with no active run / steer error:
    // Suppose run starts and another steer fails: conversion_count is already 1!
    // Or suppose we have an in-flight steer with conversion_count = 1:
    // Let's verify steer error with conversion_count 1:
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
    // Prompt was converted with conversion_count 1.
    // If prompt reply comes back as an error, prompt error prints text back.
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

    // Steer accepted with messageId
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": steer_id,
        "result": {
            "runId": "run_1",
            "messageId": "steer_msg_123"
        }
    })));

    // Run ends without seeing user_message_chunk with steer_msg_123
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

    // Notification _honk/steer_unconsumed arrives
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

    // When run ends later, it must not be reported again
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

    // Consumed steer arrives as user_message_chunk
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

    // Run ends -> NOT reported as unconsumed
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
    // 1. Normal prompt reply
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

    // 2. Normal cancelled
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

    // 3. Cancelled with not_delivered
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

    // 4. Cancelled with upstream_restart
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

    // Send a prompt, record our crid
    core.handle(Input::Line("prompt A".into()));
    let crid = core.prompt_sent().unwrap().crid.clone();
    assert_eq!(crid, "test-1");

    // Case A: run announced while PromptSent is pending -> Own
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

    // End run A
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

    // Case B: A new run starts while idle (we didn't send a prompt) -> Foreign initially
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

    // Now a synthesized user_message_chunk arrives carrying our earlier crid!
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
                            "client_req_id": "test-1"
                        }
                    }
                }
            }
        }
    })));
    // Ownership updates to Own!
    assert_eq!(core.ownership(), Ownership::Own);
}

#[test]
fn test_cancel_own_vs_foreign_vs_force_cancel_vs_idle() {
    let (mut core, _) = new_test_core();

    // 1. Idle -> /cancel / Interrupt: no-op status
    let effs = core.handle(Input::Interrupt);
    assert!(!effs.iter().any(|e| matches!(e, Effect::Notify { .. })));
    assert!(
        effs.iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("no active turn")))
    );

    let effs_cmd = core.handle(Input::Line("/cancel".into()));
    assert!(!effs_cmd.iter().any(|e| matches!(e, Effect::Notify { .. })));

    // 2. Foreign run active
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

    // /cancel! forces cancel even on foreign run
    let force_effs = core.handle(Input::Line("/cancel!".into()));
    let notify = force_effs
        .iter()
        .find(|e| matches!(e, Effect::Notify { method, .. } if method == "session/cancel"))
        .unwrap();
    let Effect::Notify { params, .. } = notify else {
        panic!()
    };
    assert_eq!(params["sessionId"], "test-session");

    // End foreign run
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

    // 3. Own run active
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
fn test_permissions_tagging_options_and_answers() {
    let (mut core, _) = new_test_core();

    // Server request session/request_permission
    let perm1 = json!({
        "jsonrpc": "2.0",
        "id": "uuid-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "shell · echo P4OK" },
            "options": [
                { "optionId": "allow_always", "name": "allow_always", "kind": "allow_always" },
                { "optionId": "allow_once", "name": "allow_once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "reject_once", "kind": "reject_once" },
                { "optionId": "reject_always", "name": "reject_always", "kind": "reject_always" }
            ]
        }
    });

    let effs1 = core.handle(Input::Frame(perm1));
    assert_eq!(core.pending_permissions().len(), 1);
    assert_eq!(core.pending_permissions()[0].tag, "r1");
    let status1 = effs1
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("[r1]")))
        .unwrap();
    let Effect::Status(msg1) = status1 else {
        panic!()
    };
    assert!(msg1.contains("shell · echo P4OK"));
    assert!(msg1.contains("1. allow_always"));

    // Second permission request (bare method name "request_permission")
    let perm2 = json!({
        "jsonrpc": "2.0",
        "id": "uuid-2",
        "method": "request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "file_write" },
            "options": [
                { "optionId": "allow", "name": "allow", "kind": "allow" },
                { "optionId": "reject", "name": "reject", "kind": "reject_once" }
            ]
        }
    });
    core.handle(Input::Frame(perm2));
    assert_eq!(core.pending_permissions().len(), 2);
    assert_eq!(core.pending_permissions()[1].tag, "r2");

    // Answer /r1 2 (allow_once)
    let ans_effs = core.handle(Input::Line("/r1 2".into()));
    let resp1 = ans_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp1 else {
        panic!()
    };
    assert_eq!(id, "uuid-1");
    assert_eq!(result["outcome"]["outcome"], "selected");
    assert_eq!(result["outcome"]["optionId"], "allow_once");
    assert_eq!(core.pending_permissions().len(), 1);

    // Answer /r2 deny
    let deny_effs = core.handle(Input::Line("/r2 deny".into()));
    let resp2 = deny_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp2 else {
        panic!()
    };
    assert_eq!(id, "uuid-2");
    assert_eq!(result["outcome"]["outcome"], "selected");
    assert_eq!(result["outcome"]["optionId"], "reject");
    assert_eq!(core.pending_permissions().len(), 0);

    // Same goose id offered again after answered: accepted with fresh tag r3
    let perm3 = json!({
        "jsonrpc": "2.0",
        "id": "uuid-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "shell · echo P4OK" },
            "options": [
                { "optionId": "allow_once", "name": "allow_once", "kind": "allow_once" }
            ]
        }
    });
    core.handle(Input::Frame(perm3));
    assert_eq!(core.pending_permissions().len(), 1);
    assert_eq!(core.pending_permissions()[0].tag, "r3");

    // Unsupported server request: never answered, reports status
    let unsupported = json!({
        "jsonrpc": "2.0",
        "id": "uuid-99",
        "method": "session/elicit_input",
        "params": {}
    });
    let unsup_effs = core.handle(Input::Frame(unsupported));
    assert!(
        !unsup_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    assert!(unsup_effs.iter().any(
        |e| matches!(e, Effect::Status(m) if m.contains("unsupported request session/elicit_input"))
    ));
}

#[test]
fn test_permissions_shorthand_n() {
    let (mut core, _) = new_test_core();

    // 1. One request pending
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "uuid-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "tool 1" },
            "options": [
                { "optionId": "opt_a", "name": "opt_a", "kind": "allow" },
                { "optionId": "opt_b", "name": "opt_b", "kind": "reject" }
            ]
        }
    })));

    // Shorthand /2 works when exactly one pending
    let ans_effs = core.handle(Input::Line("/2".into()));
    let resp = ans_effs
        .iter()
        .find(|e| matches!(e, Effect::Respond { .. }))
        .unwrap();
    let Effect::Respond { id, result } = resp else {
        panic!()
    };
    assert_eq!(id, "uuid-1");
    assert_eq!(result["outcome"]["optionId"], "opt_b");

    // 2. Now add two requests
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "uuid-2",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "tool 2" },
            "options": [{ "optionId": "opt_2", "name": "opt_2", "kind": "allow" }]
        }
    })));
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "uuid-3",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "tool 3" },
            "options": [{ "optionId": "opt_3", "name": "opt_3", "kind": "allow" }]
        }
    })));

    // Shorthand /1 refused and list re-printed!
    let refuse_effs = core.handle(Input::Line("/1".into()));
    assert!(
        !refuse_effs
            .iter()
            .any(|e| matches!(e, Effect::Respond { .. }))
    );
    let status = refuse_effs
        .iter()
        .find(|e| matches!(e, Effect::Status(m) if m.contains("shorthand /N ambiguous")))
        .unwrap();
    let Effect::Status(msg) = status else {
        panic!()
    };
    assert!(msg.contains("tool 2"));
    assert!(msg.contains("tool 3"));
}

#[test]
fn test_permissions_request_resolved_and_disconnect() {
    let (mut core, _) = new_test_core();
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "uuid-1",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "tool 1" },
            "options": []
        }
    })));
    assert_eq!(core.pending_permissions().len(), 1);

    // Notification _honk/request_resolved
    let res_effs = core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "method": "_honk/request_resolved",
        "params": {
            "sessionId": "test-session",
            "requestId": "uuid-1",
            "reason": "cancelled"
        }
    })));
    assert_eq!(core.pending_permissions().len(), 0);
    assert!(
        res_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("resolved: cancelled")))
    );

    // Offer permission again, then Disconnected
    core.handle(Input::Frame(json!({
        "jsonrpc": "2.0",
        "id": "uuid-2",
        "method": "session/request_permission",
        "params": {
            "sessionId": "test-session",
            "toolCall": { "title": "tool 2" },
            "options": []
        }
    })));
    assert_eq!(core.pending_permissions().len(), 1);

    core.handle(Input::Disconnected);
    assert_eq!(core.pending_permissions().len(), 0);
}

#[test]
fn test_commands_help_quit_slash_slash() {
    let (mut core, _) = new_test_core();

    // /help
    let help_effs = core.handle(Input::Line("/help".into()));
    assert!(
        help_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("/quit")))
    );

    // Unknown command
    let unk_effs = core.handle(Input::Line("/foobar".into()));
    assert!(
        unk_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("unknown command: '/foobar'")))
    );
    assert!(!unk_effs.iter().any(|e| matches!(e, Effect::Request { .. })));

    // //text sends literal '/text' as prompt
    let esc_effs = core.handle(Input::Line("//hello".into()));
    let req = esc_effs
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "/hello");

    // /quit -> Notify session/close and Exit(0)
    let quit_effs = core.handle(Input::Line("/quit".into()));
    assert!(
        quit_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/close"))
    );
    assert!(quit_effs.iter().any(|e| matches!(e, Effect::Exit(0))));

    // Input::Eof -> Notify session/close and Exit(0)
    let eof_effs = core.handle(Input::Eof);
    assert!(
        eof_effs
            .iter()
            .any(|e| matches!(e, Effect::Notify { method, .. } if method == "session/close"))
    );
    assert!(eof_effs.iter().any(|e| matches!(e, Effect::Exit(0))));
}

#[test]
fn test_renderer_transcript_frames_and_cursor() {
    let (mut core, _) = new_test_core();
    assert_eq!(core.cursor(), None);
    assert_eq!(core.attach_params(), json!({"sessionId": "test-session"}));

    // Live update frame with agent_message_chunk and cursor meta
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_chunk_1",
                "content": { "type": "text", "text": "Hello human!" }
            },
            "_meta": {
                "honk": {
                    "epoch": 2,
                    "seq": 10
                }
            }
        }
    });

    let effs = core.handle(Input::Frame(frame));
    let print = effs.iter().find(|e| matches!(e, Effect::Print(_))).unwrap();
    let Effect::Print(text) = print else { panic!() };
    assert!(text.contains("Hello human!"));

    assert_eq!(core.cursor(), Some(Cursor { epoch: 2, seq: 10 }));
    assert_eq!(
        core.attach_params(),
        json!({
            "sessionId": "test-session",
            "_meta": {
                "honk": {
                    "after": {
                        "epoch": 2,
                        "seq": 10
                    }
                }
            }
        })
    );

    // _honk/reset frame
    let reset_frame = json!({
        "jsonrpc": "2.0",
        "method": "_honk/reset",
        "params": {
            "sessionId": "test-session",
            "epoch": 3
        }
    });
    core.handle(Input::Frame(reset_frame));

    // _honk/replay_done frame
    let replay_done_frame = json!({
        "jsonrpc": "2.0",
        "method": "_honk/replay_done",
        "params": {
            "sessionId": "test-session",
            "epoch": 3,
            "seq": 50
        }
    });
    core.handle(Input::Frame(replay_done_frame));
    assert_eq!(core.cursor(), Some(Cursor { epoch: 3, seq: 50 }));
}

#[test]
fn test_reconnect_resends_prompt_with_same_crid() {
    let (mut core, _) = new_test_core();

    // Send prompt
    core.handle(Input::Line("prompt before disconnect".into()));
    let prompt_sent = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent.crid, "test-1");
    assert_eq!(prompt_sent.req_id, 1);

    // Also send a steer while run active to test in-flight steer warning
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

    // Disconnected arrives
    let disc_effs = core.handle(Input::Disconnected);
    assert!(
        disc_effs
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("disconnected; reconnecting...")))
    );
    assert!(disc_effs.iter().any(|e| matches!(e, Effect::Status(m) if m.contains("steer may not have been delivered: steer before disconnect"))));

    // Connected arrives -> core re-sends PromptSent with SAME crid, NEW req_id
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
    // New JSON-RPC id (not 1)
    assert_eq!(*id, 3);
    // SAME crid ("test-1")
    assert_eq!(params["_meta"]["honk"]["client_req_id"], "test-1");
    assert_eq!(params["prompt"][0]["text"], "prompt before disconnect");

    // In-flight slot updated with new req_id
    let prompt_sent_after = core.prompt_sent().unwrap();
    assert_eq!(prompt_sent_after.crid, "test-1");
    assert_eq!(prompt_sent_after.req_id, 3);
}

#[test]
fn test_idle_auto_detach_and_reattach() {
    let (mut core, now) = new_test_core();
    assert!(!core.is_detached());

    // 10 minutes pass: not detached
    let effs1 = core.handle(Input::Tick(now + std::time::Duration::from_secs(10 * 60)));
    assert!(!core.is_detached());
    assert!(!effs1.iter().any(|e| matches!(e, Effect::Detach)));

    // 31 minutes pass: idle timeout triggers Detach!
    let effs2 = core.handle(Input::Tick(now + std::time::Duration::from_secs(31 * 60)));
    assert!(core.is_detached());
    assert!(effs2.iter().any(|e| matches!(e, Effect::Detach)));
    assert!(effs2.iter().any(|e| matches!(e, Effect::Status(m) if m.contains("detached (idle); press Enter to re-attach"))));

    // Next Line while detached triggers Reattach and holds the line
    let effs3 = core.handle(Input::Line("wake up".into()));
    assert!(!core.is_detached());
    assert!(effs3.iter().any(|e| matches!(e, Effect::Reattach)));
    assert!(
        effs3
            .iter()
            .any(|e| matches!(e, Effect::Status(m) if m.contains("re-attaching...")))
    );
    // Not yet sent as prompt because connection hasn't re-established
    assert!(core.prompt_sent().is_none());

    // Connected arrives -> held line is sent!
    let effs4 = core.handle(Input::Connected);
    let prompt_req = effs4
        .iter()
        .find(|e| matches!(e, Effect::Request { method, .. } if method == "session/prompt"))
        .unwrap();
    let Effect::Request { params, .. } = prompt_req else {
        panic!()
    };
    assert_eq!(params["prompt"][0]["text"], "wake up");
}

#[test]
fn test_evicted_and_gap_frames() {
    let (mut core, _) = new_test_core();

    // _honk/gap
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

    // _honk/evicted: behaves as Disconnected + Reattach
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
