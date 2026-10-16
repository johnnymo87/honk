mod support;

use axum::http::StatusCode;
use honk::pigeon::{PigeonConfig, Reason, Registration, register};
use serde_json::json;
use support::pigeon_stub::{Behaviour, StubPigeon};

fn reg() -> Registration {
    Registration {
        goose_session_id: "20261016_7".into(),
        endpoint: "ws://127.0.0.1:4280/acp".into(),
        cwd: "/work/proj".into(),
        title: None,
        reason: Reason::NewChat,
    }
}

#[tokio::test]
async fn registers_then_announces_with_bearer_token() {
    let p = StubPigeon::start(Behaviour::default()).await;
    let cfg = PigeonConfig {
        url: p.url.clone(),
        token: Some("tok".into()),
    };

    let out = register(&cfg, &reg()).await.expect("register");
    assert_eq!(out.pigeon_session_id, "gse_test");
    assert!(out.announced);

    let seen = p.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].path, "/goose/sessions");
    assert_eq!(seen[0].auth.as_deref(), Some("Bearer tok"));
    assert_eq!(
        seen[0].body,
        json!({
            "session_id": "20261016_7",
            "endpoint": "ws://127.0.0.1:4280/acp",
            "cwd": "/work/proj",
            "label": "20261016_7",
        }),
        "no auth_token: honk's honkd secret must never be handed to pigeon"
    );

    assert_eq!(seen[1].path, "/stop");
    assert_eq!(seen[1].auth.as_deref(), Some("Bearer tok"));
    assert_eq!(seen[1].body["session_id"], "gse_test");
    assert_eq!(seen[1].body["event"], "Notification");
    assert!(seen[1].body.get("title").is_none());
    let msg = seen[1].body["message"].as_str().unwrap();
    assert!(msg.contains("20261016_7"), "{msg}");
}

#[tokio::test]
async fn no_token_sends_no_authorization_header() {
    let p = StubPigeon::start(Behaviour::default()).await;
    let cfg = PigeonConfig {
        url: p.url.clone(),
        token: None,
    };
    register(&cfg, &reg()).await.expect("register");
    assert!(p.seen().iter().all(|s| s.auth.is_none()));
}

#[tokio::test]
async fn title_is_passed_to_the_announcement() {
    let p = StubPigeon::start(Behaviour::default()).await;
    let cfg = PigeonConfig {
        url: p.url.clone(),
        token: None,
    };
    let mut r = reg();
    r.title = Some("Fix the thing".into());
    r.reason = Reason::Existing;
    register(&cfg, &r).await.expect("register");
    assert_eq!(p.seen()[1].body["title"], "Fix the thing");
}

#[tokio::test]
async fn http_error_from_register_is_reported_and_stop_not_sent() {
    let p = StubPigeon::start(Behaviour {
        register: (StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"})),
        ..Behaviour::default()
    })
    .await;
    let cfg = PigeonConfig {
        url: p.url.clone(),
        token: None,
    };
    let err = register(&cfg, &reg()).await.unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("401") && s.contains("unauthorized"), "{s}");
    assert_eq!(p.seen().len(), 1);
}

#[tokio::test]
async fn quieted_announcement_is_registered_but_not_announced() {
    let p = StubPigeon::start(Behaviour {
        stop: (
            StatusCode::OK,
            json!({"ok": true, "notified": false, "reason": "quiet_origin"}),
        ),
        ..Behaviour::default()
    })
    .await;
    let cfg = PigeonConfig {
        url: p.url.clone(),
        token: None,
    };
    let out = register(&cfg, &reg()).await.expect("register");
    assert!(!out.announced);
    assert_eq!(out.not_announced_reason.as_deref(), Some("quiet_origin"));
    assert!(out.summary().contains("quiet_origin"));
}

#[tokio::test]
async fn unreachable_pigeon_is_an_error() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let cfg = PigeonConfig { url, token: None };
    let err = register(&cfg, &reg()).await.unwrap_err();
    assert!(format!("{err:#}").contains("pigeon unreachable"), "{err:#}");
}
