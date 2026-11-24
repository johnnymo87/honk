use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use honk_upstream::Upstream;
use serde_json::Value;

/// Finds an entry in `reply["configOptions"]` (array) where `id == "model"`
/// and returns `currentValue` as `&str`.
pub fn extract_current_model(reply: &Value) -> Option<&str> {
    let opts = reply
        .as_array()
        .or_else(|| reply.get("configOptions").and_then(Value::as_array))
        .or_else(|| reply.pointer("/result/configOptions").and_then(Value::as_array))?;
    for opt in opts {
        if opt.get("id").and_then(Value::as_str) == Some("model") {
            return opt.get("currentValue").and_then(Value::as_str);
        }
    }
    None
}

fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

/// Invokes the optional external on-load model hook when an unheld session is activated.
///
/// 1. Runs `<cmd> decide --session <sid> [--current <current_model>]`.
/// 2. If a non-empty target model differing from current is returned, calls
///    upstream `session/set_config_option`.
/// 3. If the switch succeeds (or times out ambiguously), runs
///    `<cmd> applied --session <sid> --from <from> --to <to> --ts-ms <ts_ms>`.
pub async fn maybe_switch_model_on_load(
    cmd: &Path,
    decide_timeout: Duration,
    rpc_timeout: Duration,
    applied_timeout: Duration,
    sid: &str,
    session_upstream: &mut Upstream,
    load_reply: &mut Value,
) {
    let current_model = extract_current_model(load_reply).map(str::to_string);

    let mut decide_cmd = tokio::process::Command::new(cmd);
    decide_cmd.arg("decide").arg("--session").arg(sid);
    if let Some(cur) = &current_model {
        decide_cmd.arg("--current").arg(cur);
    }
    decide_cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match decide_cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                %sid,
                command = %cmd.display(),
                "on-load model hook decide spawn error: {e:#}"
            );
            return;
        }
    };

    let output = match tokio::time::timeout(decide_timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(
                %sid,
                command = %cmd.display(),
                "on-load model hook decide wait error: {e:#}"
            );
            return;
        }
        Err(_) => {
            tracing::warn!(
                %sid,
                command = %cmd.display(),
                "on-load model hook decide timed out after {decide_timeout:?}"
            );
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_trunc = truncate_str(&stderr, 500);
        tracing::warn!(
            %sid,
            command = %cmd.display(),
            status = %output.status,
            stderr = %stderr_trunc,
            "on-load model hook decide exited with non-zero status"
        );
        return;
    }

    let target = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    if target.is_empty() || current_model.as_deref() == Some(target.as_str()) {
        return;
    }

    let ts_ms = crate::turn_hook::now_ms();

    let mut notify_applied = false;
    match session_upstream
        .call_full(
            "session/set_config_option",
            serde_json::json!({
                "sessionId": sid,
                "configId": "model",
                "value": &target,
            }),
            Some(sid),
            rpc_timeout,
        )
        .await
    {
        Ok((_, frame)) => {
            if let Some(err) = frame.get("error") {
                tracing::warn!(
                    %sid,
                    %target,
                    "on-load model hook upstream session/set_config_option returned error: {err}"
                );
            } else {
                let result = frame.get("result").unwrap_or(&frame);
                if extract_current_model(result) == Some(target.as_str()) {
                    if let Some(opts) = result.get("configOptions") {
                        let obj = match load_reply.as_object_mut() {
                            Some(o) => o,
                            None => {
                                *load_reply = serde_json::json!({});
                                load_reply.as_object_mut().expect("object")
                            }
                        };
                        obj.insert("configOptions".to_string(), opts.clone());
                    }
                    tracing::info!(
                        %sid,
                        from = ?current_model,
                        to = %target,
                        "on-load model hook switched model successfully"
                    );
                    notify_applied = true;
                } else {
                    tracing::warn!(
                        %sid,
                        %target,
                        "on-load model hook upstream session/set_config_option reply model mismatch"
                    );
                }
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("timed out") {
                tracing::warn!(
                    %sid,
                    %target,
                    "on-load model hook upstream session/set_config_option timed out (ambiguous): {e:#}"
                );
                notify_applied = true;
            } else {
                tracing::warn!(
                    %sid,
                    %target,
                    "on-load model hook upstream session/set_config_option transport error: {e:#}"
                );
            }
        }
    }

    if notify_applied {
        let mut applied_cmd = tokio::process::Command::new(cmd);
        applied_cmd
            .arg("applied")
            .arg("--session")
            .arg(sid)
            .arg("--from")
            .arg(current_model.as_deref().unwrap_or(""))
            .arg("--to")
            .arg(&target)
            .arg("--ts-ms")
            .arg(ts_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        match applied_cmd.spawn() {
            Ok(child) => {
                match tokio::time::timeout(applied_timeout, child.wait_with_output()).await {
                    Ok(Ok(out)) => {
                        if !out.status.success() {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            let stderr_trunc = truncate_str(&stderr, 500);
                            tracing::warn!(
                                %sid,
                                command = %cmd.display(),
                                status = %out.status,
                                stderr = %stderr_trunc,
                                "on-load model hook applied exited with non-zero status"
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            %sid,
                            command = %cmd.display(),
                            "on-load model hook applied wait error: {e:#}"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            %sid,
                            command = %cmd.display(),
                            "on-load model hook applied timed out after {applied_timeout:?}"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    %sid,
                    command = %cmd.display(),
                    "on-load model hook applied spawn error: {e:#}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_current_model_from_object() {
        let val = json!({
            "configOptions": [
                { "id": "provider", "currentValue": "openai" },
                { "id": "model", "currentValue": "gpt-4o" }
            ]
        });
        assert_eq!(extract_current_model(&val), Some("gpt-4o"));
    }

    #[test]
    fn test_extract_current_model_from_result() {
        let val = json!({
            "result": {
                "configOptions": [
                    { "id": "model", "currentValue": "claude-3-5-sonnet" }
                ]
            }
        });
        assert_eq!(extract_current_model(&val), Some("claude-3-5-sonnet"));
    }

    #[test]
    fn test_extract_current_model_from_array() {
        let val = json!([
            { "id": "model", "currentValue": "gemini-1.5-pro" }
        ]);
        assert_eq!(extract_current_model(&val), Some("gemini-1.5-pro"));
    }

    #[test]
    fn test_extract_current_model_missing() {
        let val = json!({
            "configOptions": [
                { "id": "provider", "currentValue": "openai" }
            ]
        });
        assert_eq!(extract_current_model(&val), None);

        let empty = json!({});
        assert_eq!(extract_current_model(&empty), None);
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("🦀🦀🦀", 4), "🦀");
        assert_eq!(truncate_str("🦀🦀🦀", 5), "🦀");
        assert_eq!(truncate_str("🦀🦀🦀", 8), "🦀🦀");
    }
}
