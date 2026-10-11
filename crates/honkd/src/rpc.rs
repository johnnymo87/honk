use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Request,
    Notification,
    Response,
    Invalid,
}

pub fn kind(value: &Value) -> Kind {
    let Some(obj) = value.as_object() else {
        return Kind::Invalid;
    };

    if let Some(jsonrpc) = obj.get("jsonrpc").and_then(Value::as_str)
        && jsonrpc != "2.0"
    {
        return Kind::Invalid;
    }

    if let Some(_method) = obj.get("method").and_then(Value::as_str) {
        if obj.get("id").is_none_or(Value::is_null) {
            Kind::Notification
        } else {
            Kind::Request
        }
    } else if (obj.contains_key("result") || obj.contains_key("error"))
        && obj.get("id").is_some_and(|id| !id.is_null())
    {
        Kind::Response
    } else {
        Kind::Invalid
    }
}

pub fn session_id(value: &Value) -> Option<&str> {
    value
        .pointer("/params/sessionId")
        .or_else(|| value.pointer("/result/sessionId"))
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
}

pub fn response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub fn error(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut err = serde_json::json!({
        "code": code,
        "message": message.into(),
    });
    if let Some(d) = data {
        err["data"] = d;
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": err,
    })
}

pub fn notification(method: impl Into<String>, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method.into(),
        "params": params,
    })
}

pub fn strip_honk_meta(value: &mut Value) -> &mut Value {
    if let Some(top) = value.as_object_mut()
        && let Some(obj) = top.get_mut("_meta").and_then(Value::as_object_mut)
    {
        obj.remove("honk");
        if obj.is_empty() {
            top.remove("_meta");
        }
    }
    if let Some(params) = value.get_mut("params").and_then(Value::as_object_mut)
        && let Some(obj) = params.get_mut("_meta").and_then(Value::as_object_mut)
    {
        obj.remove("honk");
        if obj.is_empty() {
            params.remove("_meta");
        }
    }
    value
}

pub fn busy_run_id(val: &Value) -> Option<String> {
    let s = val
        .pointer("/error/data")
        .or_else(|| val.get("data"))
        .or_else(|| val.pointer("/error/message"))
        .or_else(|| val.get("message"))
        .and_then(Value::as_str)
        .or_else(|| val.as_str())?;

    const PREFIX: &str = "session already has active run `";
    let start = s.find(PREFIX)? + PREFIX.len();
    let rest = &s[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

pub fn busy_error(run: &str) -> Value {
    serde_json::json!({
        "code": -32602,
        "message": "Invalid params",
        "data": format!("session already has active run `{run}`; use _goose/unstable/session/steer"),
    })
}

pub fn is_no_active_run(val: &Value) -> bool {
    let check_str = |s: &str| s.contains("no active run");

    val.pointer("/error/data")
        .and_then(Value::as_str)
        .is_some_and(check_str)
        || val
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(check_str)
        || val
            .pointer("/error/data/message")
            .and_then(Value::as_str)
            .is_some_and(check_str)
        || val
            .pointer("/data/message")
            .and_then(Value::as_str)
            .is_some_and(check_str)
        || val
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(check_str)
        || val
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(check_str)
        || val.as_str().is_some_and(check_str)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamIds {
    pub boot: u64,
    pub next_seq: u64,
}

impl UpstreamIds {
    pub fn new(boot: u64) -> Self {
        Self { boot, next_seq: 1 }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> String {
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        let n = self.next_seq;
        self.next_seq += 1;
        format!("hk{}-{n}", self.boot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_kind() {
        let req = json!({"jsonrpc": "2.0", "method": "session/prompt", "id": 1, "params": {}});
        assert_eq!(kind(&req), Kind::Request);

        let notif = json!({"jsonrpc": "2.0", "method": "session/update", "params": {}});
        assert_eq!(kind(&notif), Kind::Notification);

        let notif_null_id =
            json!({"jsonrpc": "2.0", "method": "session/update", "id": null, "params": {}});
        assert_eq!(kind(&notif_null_id), Kind::Notification);

        let resp_ok = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert_eq!(kind(&resp_ok), Kind::Response);

        let resp_err =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "error"}});
        assert_eq!(kind(&resp_err), Kind::Response);

        let invalid_garbage = json!("not an object");
        assert_eq!(kind(&invalid_garbage), Kind::Invalid);

        let invalid_no_method_or_result = json!({"jsonrpc": "2.0", "id": 1});
        assert_eq!(kind(&invalid_no_method_or_result), Kind::Invalid);

        let invalid_jsonrpc_ver = json!({"jsonrpc": "1.0", "method": "foo"});
        assert_eq!(kind(&invalid_jsonrpc_ver), Kind::Invalid);
    }

    #[test]
    fn test_session_id() {
        let req = json!({"params": {"sessionId": "sid_123"}});
        assert_eq!(session_id(&req), Some("sid_123"));

        let res = json!({"result": {"sessionId": "sid_456"}});
        assert_eq!(session_id(&res), Some("sid_456"));

        let top = json!({"sessionId": "sid_789"});
        assert_eq!(session_id(&top), Some("sid_789"));

        let none = json!({"params": {}});
        assert_eq!(session_id(&none), None);
    }

    #[test]
    fn test_response_and_error_and_notification() {
        let resp = response(json!(42), json!({"status": "ok"}));
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
        assert_eq!(resp["result"]["status"], "ok");

        let err = error(json!(42), -32602, "Invalid params", Some(json!("detail")));
        assert_eq!(err["jsonrpc"], "2.0");
        assert_eq!(err["id"], 42);
        assert_eq!(err["error"]["code"], -32602);
        assert_eq!(err["error"]["message"], "Invalid params");
        assert_eq!(err["error"]["data"], "detail");

        let notif = notification("session/update", json!({"sessionId": "s1"}));
        assert_eq!(notif["jsonrpc"], "2.0");
        assert_eq!(notif["method"], "session/update");
        assert_eq!(notif["params"]["sessionId"], "s1");
        assert!(notif.get("id").is_none());
    }

    #[test]
    fn test_strip_honk_meta() {
        let mut val = json!({
            "params": {
                "sessionId": "s1",
                "_meta": {
                    "honk": { "client_req_id": "c1", "after": { "epoch": 1, "seq": 2 } },
                    "goose": { "steer": true }
                }
            }
        });
        strip_honk_meta(&mut val);
        assert!(val.pointer("/params/_meta/honk").is_none());
        assert_eq!(val.pointer("/params/_meta/goose/steer"), Some(&json!(true)));

        let mut top_meta = json!({
            "_meta": {
                "honk": { "client_req_id": "c1" },
                "goose": { "activeRunId": "run_1" }
            }
        });
        strip_honk_meta(&mut top_meta);
        assert!(top_meta.pointer("/_meta/honk").is_none());
        assert_eq!(
            top_meta.pointer("/_meta/goose/activeRunId"),
            Some(&json!("run_1"))
        );
    }

    #[test]
    fn test_strip_honk_meta_removes_empty_meta() {
        let mut val = json!({
            "_meta": {
                "honk": { "client_req_id": "c1" }
            },
            "params": {
                "_meta": {
                    "honk": { "client_req_id": "c2" }
                }
            }
        });
        strip_honk_meta(&mut val);
        assert!(val.get("_meta").is_none());
        assert!(val.pointer("/params/_meta").is_none());
    }

    #[test]
    fn test_busy_error_and_busy_run_id() {
        let run = "run_6f2cb817-93a3-4e79-8fa8-891115c3fe6a";
        let err = busy_error(run);
        assert_eq!(err["code"], -32602);
        assert_eq!(
            err["data"],
            format!("session already has active run `{run}`; use _goose/unstable/session/steer")
        );

        // Exact goose error string
        let exact_goose_str = json!({
            "code": -32602,
            "message": "Invalid params",
            "data": "session already has active run `run_6f2cb817-93a3-4e79-8fa8-891115c3fe6a`; use _goose/unstable/session/steer"
        });
        assert_eq!(busy_run_id(&exact_goose_str), Some(run.to_string()));

        // On busy_error directly
        assert_eq!(busy_run_id(&err), Some(run.to_string()));

        // In a full JSON-RPC response
        let full_resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": err
        });
        assert_eq!(busy_run_id(&full_resp), Some(run.to_string()));

        // Non-busy error
        let other_err = json!({"code": -32600, "message": "other error"});
        assert_eq!(busy_run_id(&other_err), None);
    }

    #[test]
    fn test_is_no_active_run() {
        let err1 = json!({"data": "no active run to steer"});
        assert!(is_no_active_run(&err1));

        let err2 = json!({
            "error": {
                "code": -32602,
                "data": "no active run to steer"
            }
        });
        assert!(is_no_active_run(&err2));

        assert!(!is_no_active_run(&json!({"data": "something else"})));
        assert!(!is_no_active_run(&json!({"code": -32600})));
    }

    #[test]
    fn test_upstream_ids() {
        let mut ids = UpstreamIds::new(42);
        assert_eq!(ids.next(), "hk42-1");
        assert_eq!(ids.next(), "hk42-2");
        assert_eq!(ids.next(), "hk42-3");
    }
}
