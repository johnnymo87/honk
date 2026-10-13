use serde_json::Value;

pub fn is_busy_error(val: &Value) -> bool {
    let check = |s: &str| s.contains("already has active run");
    val.pointer("/data")
        .and_then(Value::as_str)
        .is_some_and(check)
        || val
            .pointer("/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val
            .pointer("/data/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val.as_str().is_some_and(check)
}

pub fn busy_run_id(val: &Value) -> Option<String> {
    let s = val
        .pointer("/data")
        .and_then(Value::as_str)
        .or_else(|| val.pointer("/message").and_then(Value::as_str))
        .or_else(|| val.pointer("/data/message").and_then(Value::as_str))
        .or_else(|| val.as_str())?;
    const PREFIX: &str = "session already has active run `";
    let start = s.find(PREFIX)? + PREFIX.len();
    let rest = &s[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

pub fn is_no_active_run(val: &Value) -> bool {
    let check = |s: &str| s.contains("no active run");
    val.pointer("/data")
        .and_then(Value::as_str)
        .is_some_and(check)
        || val
            .pointer("/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val
            .pointer("/data/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val.as_str().is_some_and(check)
}

pub fn is_run_mismatch(val: &Value) -> bool {
    if val.pointer("/data/actualRunId").is_some() || val.pointer("/data/expectedRunId").is_some() {
        return true;
    }
    let check = |s: &str| s.contains("mismatch") || s.contains("expected active run id");
    val.pointer("/data")
        .and_then(Value::as_str)
        .is_some_and(check)
        || val
            .pointer("/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val
            .pointer("/data/message")
            .and_then(Value::as_str)
            .is_some_and(check)
}

pub fn is_lost_track(val: &Value) -> bool {
    let check = |s: &str| s.contains("lost track");
    val.pointer("/data")
        .and_then(Value::as_str)
        .is_some_and(check)
        || val
            .pointer("/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val
            .pointer("/data/message")
            .and_then(Value::as_str)
            .is_some_and(check)
        || val.as_str().is_some_and(check)
}
