//! MCP session operations: list, enable, and disable.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::future::Future;

use crate::mcp::catalog::Catalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub message: String,
}

impl RequestError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RequestError {}

pub trait GooseRequester: Send + Sync {
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, RequestError>> + Send;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub no_confirm: BTreeSet<String>,
}

impl Policy {
    pub fn new(no_confirm: impl IntoIterator<Item = String>) -> Self {
        Self {
            no_confirm: no_confirm.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpError {
    pub code: i32,
    pub message: String,
}

impl OpError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    pub fn catalog_unreadable(err: impl std::fmt::Display) -> Self {
        Self {
            code: -32603,
            message: format!("MCP catalog unreadable: {err}"),
        }
    }

    pub fn request_failed(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for OpError {}

impl From<RequestError> for OpError {
    fn from(err: RequestError) -> Self {
        Self::request_failed(err.message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub enabled: bool,
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sibling: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtherExtension {
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListResult {
    pub servers: Vec<ServerInfo>,
    pub other: Vec<OtherExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NameResult {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<String>,
    #[serde(default)]
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpResult {
    pub results: Vec<NameResult>,
    pub servers: Vec<ServerInfo>,
    pub other: Vec<OtherExtension>,
    pub enabled: Vec<String>,
    pub summary: String,
}

impl OpResult {
    pub fn has_changes(&self) -> bool {
        self.results.iter().any(|r| r.changed)
    }
}

fn find_sibling(name: &str, catalog: &Catalog) -> Option<String> {
    if let Some(base) = name.strip_suffix("-ro") {
        if catalog.get(base).is_some() {
            return Some(base.to_string());
        }
    } else {
        let candidate = format!("{name}-ro");
        if catalog.get(&candidate).is_some() {
            return Some(candidate);
        }
    }
    None
}

struct GooseExtensionInfo {
    key: String,
    description: Option<String>,
}

async fn fetch_goose_extensions(
    req: &impl GooseRequester,
    session_id: &str,
) -> Result<Vec<GooseExtensionInfo>, OpError> {
    let resp = req
        .request(
            "_goose/unstable/session/extensions/list",
            serde_json::json!({ "sessionId": session_id }),
        )
        .await?;

    let exts = resp
        .get("extensions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut list = Vec::new();
    for ext_val in exts {
        let key = ext_val
            .get("extensionKey")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string();
        if key.is_empty() {
            continue;
        }

        // Description is in ext_val["extension"]["description"], or top-level ext_val["description"]
        let desc = ext_val
            .get("extension")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| ext_val.get("description").and_then(|d| d.as_str()))
            .map(|s| s.to_string());

        list.push(GooseExtensionInfo {
            key,
            description: desc,
        });
    }
    Ok(list)
}

/// Lists catalog MCP servers and their status in the goose session.
///
/// An MCP server is considered `enabled` if and only if goose's extension list has
/// an entry with `extensionKey == name` AND `description == marker(name)`.
///
/// If an extension with `extensionKey == name` exists but lacks the marker, it is reported
/// as `conflict: true` (and `enabled: false`) to protect foreign or platform extensions
/// from being clobbered or mistaken for honk-managed extensions.
pub async fn list(
    req: &impl GooseRequester,
    catalog: &Catalog,
    policy: &Policy,
    session_id: &str,
) -> Result<ListResult, OpError> {
    let goose_exts = fetch_goose_extensions(req, session_id).await?;

    let mut servers = Vec::new();
    let mut matched_keys = HashSet::new();

    for entry in &catalog.entries {
        let name = &entry.name;
        let expected_marker = crate::mcp::catalog::marker(name);

        let goose_match = goose_exts.iter().find(|e| &e.key == name);
        let (enabled, conflict) = match goose_match {
            Some(ext) => {
                let has_marker = ext.description.as_deref() == Some(&expected_marker);
                if has_marker {
                    matched_keys.insert(name.clone());
                    (true, None)
                } else {
                    (false, Some(true))
                }
            }
            None => (false, None),
        };

        let supported = entry.supported.is_ok();
        let reason = entry.supported.as_ref().err().cloned();
        let confirm = !policy.no_confirm.contains(name);
        let sibling = find_sibling(name, catalog);

        servers.push(ServerInfo {
            name: name.clone(),
            enabled,
            supported,
            reason,
            confirm,
            conflict,
            sibling,
        });
    }

    let mut other = Vec::new();
    for ext in &goose_exts {
        if !matched_keys.contains(&ext.key) {
            other.push(OtherExtension {
                key: ext.key.clone(),
            });
        }
    }

    Ok(ListResult { servers, other })
}

fn format_enable_summary(results: &[NameResult]) -> String {
    let mut parts = Vec::new();
    for r in results {
        if r.ok {
            if let Some(note) = &r.note {
                parts.push(format!("{} ({note})", r.name));
            } else if let Some(replaced) = &r.replaced {
                parts.push(format!("enabled {}; replaced {replaced}", r.name));
            } else {
                parts.push(format!("enabled {}", r.name));
            }
        } else {
            parts.push(format!("failed to enable {}", r.name));
        }
    }
    if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join("; ")
    }
}

/// Enables the specified MCP servers in the given goose session.
///
/// Validates all server names before executing any requests. If any server name requires
/// confirmation and `confirm` is false, returns an invalid params error guiding the user.
///
/// If an exclusive sibling (e.g. `slack-ro` when enabling `slack`, or vice versa) is currently
/// enabled with the marker, it is removed before adding the new server.
pub async fn enable(
    req: &impl GooseRequester,
    catalog: &Catalog,
    policy: &Policy,
    session_id: &str,
    names: &[String],
    confirm: &[String],
) -> Result<OpResult, OpError> {
    if names.is_empty() {
        return Err(OpError::invalid_params("no MCP servers specified"));
    }

    // Dedupe names preserving order
    let mut unique_names = Vec::new();
    let mut seen = HashSet::new();
    for n in names {
        if seen.insert(n) {
            unique_names.push(n.clone());
        }
    }

    // Validate ALL names first
    for name in &unique_names {
        let entry = match catalog.get(name) {
            Some(e) => e,
            None => {
                return Err(OpError::invalid_params(format!(
                    "unknown MCP server '{name}'; /mcp lists them"
                )));
            }
        };

        if let Err(reason) = &entry.supported {
            return Err(OpError::invalid_params(format!(
                "'{name}' is not supported: {reason}"
            )));
        }

        if let Some(sibling) = find_sibling(name, catalog)
            && unique_names.contains(&sibling)
        {
            return Err(OpError::invalid_params(format!(
                "cannot enable both '{name}' and its sibling '{sibling}' in the same request"
            )));
        }

        if !policy.no_confirm.contains(name) && !confirm.iter().any(|c| c == name) {
            let mut msg = format!("'{name}' can write; use /mcp on! {name} to confirm");
            if let Some(sibling) = find_sibling(name, catalog)
                && sibling.ends_with("-ro")
            {
                msg.push_str(&format!(" (or /mcp on {sibling} for read-only)"));
            }
            return Err(OpError::invalid_params(msg));
        }
    }

    let mut goose_exts = fetch_goose_extensions(req, session_id).await?;

    let mut results = Vec::new();
    for name in &unique_names {
        let expected_marker = crate::mcp::catalog::marker(name);
        let existing = goose_exts.iter().find(|e| &e.key == name);

        if let Some(ext) = existing {
            if ext.description.as_deref() == Some(&expected_marker) {
                // Already on
                results.push(NameResult {
                    name: name.clone(),
                    ok: true,
                    error: None,
                    note: Some("already on".to_string()),
                    replaced: None,
                    changed: false,
                });
                continue;
            } else {
                // Conflict
                results.push(NameResult {
                    name: name.clone(),
                    ok: false,
                    error: Some(format!(
                        "an extension named '{name}' that honk did not add is already enabled"
                    )),
                    note: None,
                    replaced: None,
                    changed: false,
                });
                continue;
            }
        }

        // Check if sibling is currently enabled WITH marker
        let mut replaced = None;
        if let Some(sibling) = find_sibling(name, catalog) {
            let sibling_marker = crate::mcp::catalog::marker(&sibling);
            let sibling_enabled = goose_exts
                .iter()
                .any(|e| e.key == sibling && e.description.as_deref() == Some(&sibling_marker));

            if sibling_enabled {
                // Remove sibling first
                let remove_res = req
                    .request(
                        "_goose/unstable/session/extensions/remove",
                        serde_json::json!({
                            "sessionId": session_id,
                            "extensionKey": sibling,
                        }),
                    )
                    .await;
                match remove_res {
                    Ok(_) => {
                        goose_exts.retain(|e| e.key != sibling);
                        replaced = Some(sibling);
                    }
                    Err(e) => {
                        results.push(NameResult {
                            name: name.clone(),
                            ok: false,
                            error: Some(format!("failed to replace '{sibling}': {}", e.message)),
                            note: None,
                            replaced: None,
                            changed: false,
                        });
                        continue;
                    }
                }
            }
        }

        // Add extension
        let spec = catalog.get(name).unwrap().supported.as_ref().unwrap();
        let ext_json = spec.to_add_extension_json();
        let add_res = req
            .request(
                "_goose/unstable/session/extensions/add",
                serde_json::json!({
                    "sessionId": session_id,
                    "extension": ext_json,
                }),
            )
            .await;

        match add_res {
            Ok(_) => {
                goose_exts.retain(|e| &e.key != name);
                goose_exts.push(GooseExtensionInfo {
                    key: name.clone(),
                    description: Some(expected_marker),
                });
                results.push(NameResult {
                    name: name.clone(),
                    ok: true,
                    error: None,
                    note: None,
                    replaced,
                    changed: true,
                });
            }
            Err(e) => {
                let changed = replaced.is_some();
                let redacted_error = crate::mcp::redact::redact_mcp_error(&e.message, spec);
                results.push(NameResult {
                    name: name.clone(),
                    ok: false,
                    error: Some(redacted_error),
                    note: None,
                    replaced,
                    changed,
                });
            }
        }
    }

    // Re-list
    let list_res = list(req, catalog, policy, session_id).await?;
    let enabled: Vec<String> = list_res
        .servers
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.name.clone())
        .collect();

    let summary = format_enable_summary(&results);

    Ok(OpResult {
        results,
        servers: list_res.servers,
        other: list_res.other,
        enabled,
        summary,
    })
}

fn format_disable_summary(results: &[NameResult]) -> String {
    let mut parts = Vec::new();
    for r in results {
        if r.ok {
            if let Some(note) = &r.note {
                parts.push(format!("{} ({note})", r.name));
            } else {
                parts.push(format!("disabled {}", r.name));
            }
        } else {
            parts.push(format!("failed to disable {}", r.name));
        }
    }
    if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join("; ")
    }
}

/// Disables the specified MCP servers in the given goose session.
///
/// Only removes extensions carrying the `honk-mcp:<name>` marker. Builtin and platform
/// extensions (like `developer`) and un-marked extensions are never removed by this function,
/// ensuring other clients' extensions remain protected.
pub async fn disable(
    req: &impl GooseRequester,
    catalog: &Catalog,
    policy: &Policy,
    session_id: &str,
    names: &[String],
) -> Result<OpResult, OpError> {
    if names.is_empty() {
        return Err(OpError::invalid_params("no MCP servers specified"));
    }

    let mut unique_names = Vec::new();
    let mut seen = HashSet::new();
    for n in names {
        if seen.insert(n) {
            unique_names.push(n.clone());
        }
    }

    for name in &unique_names {
        if catalog.get(name).is_none() {
            return Err(OpError::invalid_params(format!(
                "unknown MCP server '{name}'; /mcp lists them"
            )));
        }
    }

    let mut goose_exts = fetch_goose_extensions(req, session_id).await?;

    let mut results = Vec::new();
    for name in &unique_names {
        let expected_marker = crate::mcp::catalog::marker(name);
        let existing = goose_exts.iter().find(|e| &e.key == name);

        match existing {
            Some(ext) => {
                if ext.description.as_deref() == Some(&expected_marker) {
                    let remove_res = req
                        .request(
                            "_goose/unstable/session/extensions/remove",
                            serde_json::json!({
                                "sessionId": session_id,
                                "extensionKey": name,
                            }),
                        )
                        .await;
                    match remove_res {
                        Ok(_) => {
                            goose_exts.retain(|e| &e.key != name);
                            results.push(NameResult {
                                name: name.clone(),
                                ok: true,
                                error: None,
                                note: None,
                                replaced: None,
                                changed: true,
                            });
                        }
                        Err(e) => {
                            results.push(NameResult {
                                name: name.clone(),
                                ok: false,
                                error: Some(e.message),
                                note: None,
                                replaced: None,
                                changed: false,
                            });
                        }
                    }
                } else {
                    // Conflict: exists but without marker
                    results.push(NameResult {
                        name: name.clone(),
                        ok: false,
                        error: Some("not added by honk; refusing to remove".to_string()),
                        note: None,
                        replaced: None,
                        changed: false,
                    });
                }
            }
            None => {
                // Not in goose list
                results.push(NameResult {
                    name: name.clone(),
                    ok: true,
                    error: None,
                    note: Some("already off".to_string()),
                    replaced: None,
                    changed: false,
                });
            }
        }
    }

    let list_res = list(req, catalog, policy, session_id).await?;
    let enabled: Vec<String> = list_res
        .servers
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.name.clone())
        .collect();

    let summary = format_disable_summary(&results);

    Ok(OpResult {
        results,
        servers: list_res.servers,
        other: list_res.other,
        enabled,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRequester {
        calls: Mutex<Vec<(String, Value)>>,
        extensions: Mutex<Vec<Value>>,
        fail_method: Mutex<Option<String>>,
        fail_error: Mutex<Option<String>>,
    }

    impl FakeRequester {
        fn new(initial_extensions: Vec<Value>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                extensions: Mutex::new(initial_extensions),
                fail_method: Mutex::new(None),
                fail_error: Mutex::new(None),
            }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }

        fn set_fail_method(&self, method: Option<String>) {
            *self.fail_method.lock().unwrap() = method;
        }

        fn set_fail_error(&self, err: Option<String>) {
            *self.fail_error.lock().unwrap() = err;
        }
    }

    impl GooseRequester for FakeRequester {
        async fn request(&self, method: &str, params: Value) -> Result<Value, RequestError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));

            if let Some(fail) = self.fail_method.lock().unwrap().as_ref()
                && fail == method
            {
                let msg = self
                    .fail_error
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| format!("simulated failure for {method}"));
                return Err(RequestError::new(msg));
            }

            match method {
                "_goose/unstable/session/extensions/list" => {
                    let exts = self.extensions.lock().unwrap().clone();
                    Ok(serde_json::json!({ "extensions": exts }))
                }
                "_goose/unstable/session/extensions/add" => {
                    let ext = params.get("extension").cloned().unwrap_or(Value::Null);
                    let key = ext
                        .get("server")
                        .and_then(|s| s.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown")
                        .to_string();

                    let mut exts = self.extensions.lock().unwrap();
                    exts.retain(|e| e.get("extensionKey").and_then(|k| k.as_str()) != Some(&key));
                    exts.push(serde_json::json!({
                        "extensionKey": key,
                        "extension": ext,
                    }));
                    Ok(serde_json::json!({ "ok": true }))
                }
                "_goose/unstable/session/extensions/remove" => {
                    let key = params
                        .get("extensionKey")
                        .and_then(|k| k.as_str())
                        .unwrap_or("");
                    let mut exts = self.extensions.lock().unwrap();
                    exts.retain(|e| e.get("extensionKey").and_then(|k| k.as_str()) != Some(key));
                    Ok(serde_json::json!({ "ok": true }))
                }
                _ => Err(RequestError::new(format!("unknown method {method}"))),
            }
        }
    }

    fn sample_catalog() -> Catalog {
        let json = r#"{
            "mcp": {
                "slack": {
                    "type": "local",
                    "command": ["slack-mcp"],
                    "environment": {
                        "SLACK_BOT_TOKEN": "{file:/run/secrets/token}"
                    }
                },
                "slack-ro": {
                    "type": "local",
                    "command": ["slack-mcp"],
                    "environment": {
                        "SLACK_BOT_TOKEN": "{file:/run/secrets/token}"
                    }
                },
                "pagerduty": {
                    "type": "local",
                    "command": ["pagerduty-mcp"],
                    "environment": {
                        "PD_TOKEN": "{file:/run/secrets/token}"
                    }
                },
                "figma": {
                    "type": "local",
                    "command": ["figma-mcp"],
                    "environment": {
                        "FIGMA_ACCESS_TOKEN": "{file:/run/secrets/token}"
                    }
                },
                "broken": {
                    "type": "local",
                    "command": []
                }
            }
        }"#;
        Catalog::load_from_str(json, None).unwrap()
    }

    fn sample_policy() -> Policy {
        Policy::new(["slack-ro".to_string(), "figma".to_string()])
    }

    #[tokio::test]
    async fn test_list_enabled_conflict_and_other() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let initial_exts = vec![
            // slack enabled with marker
            serde_json::json!({
                "extensionKey": "slack",
                "extension": {
                    "type": "mcp",
                    "description": "honk-mcp:slack",
                    "server": { "name": "slack" }
                }
            }),
            // figma present WITHOUT marker -> conflict
            serde_json::json!({
                "extensionKey": "figma",
                "extension": {
                    "type": "mcp",
                    "description": "manual-figma",
                    "server": { "name": "figma" }
                }
            }),
            // developer extension not in catalog -> other
            serde_json::json!({
                "extensionKey": "developer",
                "extension": {
                    "type": "builtin",
                    "description": "developer platform"
                }
            }),
        ];
        let req = FakeRequester::new(initial_exts);

        let res = list(&req, &cat, &policy, "ses_123").await.unwrap();

        let slack = res.servers.iter().find(|s| s.name == "slack").unwrap();
        assert!(slack.enabled);
        assert_eq!(slack.conflict, None);
        assert!(slack.confirm);
        assert_eq!(slack.sibling.as_deref(), Some("slack-ro"));

        let figma = res.servers.iter().find(|s| s.name == "figma").unwrap();
        assert!(!figma.enabled);
        assert_eq!(figma.conflict, Some(true));
        assert!(!figma.confirm);
        assert_eq!(figma.sibling, None);

        let slack_ro = res.servers.iter().find(|s| s.name == "slack-ro").unwrap();
        assert!(!slack_ro.enabled);
        assert_eq!(slack_ro.sibling.as_deref(), Some("slack"));

        // other should contain developer and figma (since figma is not an enabled catalog server)
        let other_keys: Vec<_> = res.other.iter().map(|o| o.key.as_str()).collect();
        assert!(other_keys.contains(&"developer"));
        assert!(other_keys.contains(&"figma"));
        assert!(!other_keys.contains(&"slack"));
    }

    #[tokio::test]
    async fn test_enable_idempotent_and_conflict() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let initial_exts = vec![
            serde_json::json!({
                "extensionKey": "figma",
                "extension": {
                    "type": "mcp",
                    "description": "honk-mcp:figma",
                    "server": { "name": "figma" }
                }
            }),
            serde_json::json!({
                "extensionKey": "slack-ro",
                "extension": {
                    "type": "mcp",
                    "description": "user-added-slack-ro",
                    "server": { "name": "slack-ro" }
                }
            }),
        ];
        let req = FakeRequester::new(initial_exts);

        // Enable figma (already enabled with marker)
        let res = enable(&req, &cat, &policy, "ses_123", &["figma".to_string()], &[])
            .await
            .unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].ok);
        assert_eq!(res.results[0].note.as_deref(), Some("already on"));

        // Enable slack-ro (held by user extension without marker -> conflict)
        let res2 = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["slack-ro".to_string()],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(res2.results.len(), 1);
        assert!(!res2.results[0].ok);
        assert!(
            res2.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("an extension named 'slack-ro' that honk did not add is already enabled")
        );
    }

    #[tokio::test]
    async fn test_sibling_replace_both_directions() {
        let cat = sample_catalog();
        let policy = sample_policy();

        // 1. Start with slack enabled. Enable slack-ro -> replaces slack
        let initial_exts = vec![serde_json::json!({
            "extensionKey": "slack",
            "extension": {
                "type": "mcp",
                "description": "honk-mcp:slack",
                "server": { "name": "slack" }
            }
        })];
        let req = FakeRequester::new(initial_exts);

        let res = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["slack-ro".to_string()],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].ok);
        assert_eq!(res.results[0].replaced.as_deref(), Some("slack"));
        assert_eq!(res.summary, "enabled slack-ro; replaced slack");

        // Verify remove slack was called before add slack-ro
        let calls = req.calls();
        let remove_call = calls.iter().find(|(m, p)| {
            m == "_goose/unstable/session/extensions/remove" && p["extensionKey"] == "slack"
        });
        assert!(remove_call.is_some());

        // 2. Start with slack-ro enabled. Enable slack (with confirm=["slack"]) -> replaces slack-ro
        let initial_exts2 = vec![serde_json::json!({
            "extensionKey": "slack-ro",
            "extension": {
                "type": "mcp",
                "description": "honk-mcp:slack-ro",
                "server": { "name": "slack-ro" }
            }
        })];
        let req2 = FakeRequester::new(initial_exts2);

        let res2 = enable(
            &req2,
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &["slack".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(res2.results.len(), 1);
        assert!(res2.results[0].ok);
        assert_eq!(res2.results[0].replaced.as_deref(), Some("slack-ro"));
        assert_eq!(res2.summary, "enabled slack; replaced slack-ro");
    }

    #[tokio::test]
    async fn test_enable_rejects_mutual_siblings_in_same_request() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let req = FakeRequester::new(vec![]);

        let err = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string(), "slack-ro".to_string()],
            &["slack".to_string()],
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, -32602);
        assert!(
            err.message
                .contains("cannot enable both 'slack' and its sibling 'slack-ro'")
        );
        assert!(req.calls().is_empty());
    }

    #[tokio::test]
    async fn test_enable_loop_uses_fresh_state_between_items() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let initial_exts = vec![serde_json::json!({
            "extensionKey": "slack-ro",
            "extension": {
                "type": "mcp",
                "description": "honk-mcp:slack-ro",
                "server": { "name": "slack-ro" }
            }
        })];
        let req = FakeRequester::new(initial_exts);

        let res = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string(), "figma".to_string()],
            &["slack".to_string()],
        )
        .await
        .unwrap();

        assert_eq!(res.results.len(), 2);
        assert!(res.results[0].ok);
        assert_eq!(res.results[0].replaced.as_deref(), Some("slack-ro"));
        assert!(res.results[0].changed);
        assert!(res.results[1].ok);
        assert!(res.results[1].changed);
        assert!(res.has_changes());
    }

    #[tokio::test]
    async fn test_has_changes_flag() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let initial_exts = vec![serde_json::json!({
            "extensionKey": "figma",
            "extension": {
                "type": "mcp",
                "description": "honk-mcp:figma",
                "server": { "name": "figma" }
            }
        })];
        let req = FakeRequester::new(initial_exts);

        // 1. Enable figma (already on) -> has_changes is false
        let res1 = enable(&req, &cat, &policy, "ses_123", &["figma".to_string()], &[])
            .await
            .unwrap();
        assert!(!res1.has_changes());
        assert!(!res1.results[0].changed);

        // 2. Disable figma -> has_changes is true
        let res2 = disable(&req, &cat, &policy, "ses_123", &["figma".to_string()])
            .await
            .unwrap();
        assert!(res2.has_changes());
        assert!(res2.results[0].changed);

        // 3. Disable figma again (already off) -> has_changes is false
        let res3 = disable(&req, &cat, &policy, "ses_123", &["figma".to_string()])
            .await
            .unwrap();
        assert!(!res3.has_changes());
        assert!(!res3.results[0].changed);
    }

    #[tokio::test]
    async fn test_confirm_refusal_and_hint() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let req = FakeRequester::new(vec![]);

        // slack requires confirm and has slack-ro sibling
        let err1 = enable(&req, &cat, &policy, "ses_123", &["slack".to_string()], &[])
            .await
            .unwrap_err();
        assert_eq!(err1.code, -32602);
        assert_eq!(
            err1.message,
            "'slack' can write; use /mcp on! slack to confirm (or /mcp on slack-ro for read-only)"
        );

        // pagerduty requires confirm but has NO -ro sibling
        let err2 = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["pagerduty".to_string()],
            &[],
        )
        .await
        .unwrap_err();
        assert_eq!(err2.code, -32602);
        assert_eq!(
            err2.message,
            "'pagerduty' can write; use /mcp on! pagerduty to confirm"
        );

        // slack with confirm: ["slack"] succeeds
        let res_slack = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &["slack".to_string()],
        )
        .await
        .unwrap();
        assert!(res_slack.results[0].ok);

        // figma is in no_confirm -> succeeds without confirm
        let res_figma = enable(&req, &cat, &policy, "ses_123", &["figma".to_string()], &[])
            .await
            .unwrap();
        assert!(res_figma.results[0].ok);

        // Fail-safe default policy (empty no_confirm): EVERY server requires confirmation
        let default_policy = Policy::default();
        let err_default = enable(
            &req,
            &cat,
            &default_policy,
            "ses_123",
            &["figma".to_string()],
            &[],
        )
        .await
        .unwrap_err();
        assert_eq!(err_default.code, -32602);
        assert_eq!(
            err_default.message,
            "'figma' can write; use /mcp on! figma to confirm"
        );
    }

    #[tokio::test]
    async fn test_validation_refusals_perform_no_requests() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let req = FakeRequester::new(vec![]);

        // Unknown name
        let err_unknown = enable(
            &req,
            &cat,
            &policy,
            "ses_123",
            &["unknown_srv".to_string()],
            &[],
        )
        .await
        .unwrap_err();
        assert_eq!(err_unknown.code, -32602);
        assert_eq!(
            err_unknown.message,
            "unknown MCP server 'unknown_srv'; /mcp lists them"
        );
        assert!(req.calls().is_empty());

        // Unsupported name
        let err_unsupported = enable(&req, &cat, &policy, "ses_123", &["broken".to_string()], &[])
            .await
            .unwrap_err();
        assert_eq!(err_unsupported.code, -32602);
        assert!(
            err_unsupported
                .message
                .contains("'broken' is not supported")
        );
        assert!(req.calls().is_empty());

        // Empty names
        let err_empty = enable(&req, &cat, &policy, "ses_123", &[], &[])
            .await
            .unwrap_err();
        assert_eq!(err_empty.code, -32602);
        assert!(req.calls().is_empty());
    }

    #[tokio::test]
    async fn test_per_name_add_failure_does_not_abort_others() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let req = FakeRequester::new(vec![]);
        req.set_fail_method(Some("_goose/unstable/session/extensions/add".to_string()));

        let res = enable(&req, &cat, &policy, "ses_123", &["figma".to_string()], &[])
            .await
            .unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(!res.results[0].ok);
        assert!(res.results[0].error.is_some());
    }

    #[tokio::test]
    async fn test_enable_per_name_add_failure_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("token");
        std::fs::write(&secret_file, "secret_key_12345\n").unwrap();

        let json = format!(
            r#"{{
            "mcp": {{
                "custom": {{
                    "type": "local",
                    "command": ["custom-mcp"],
                    "environment": {{
                        "TOKEN": "{{file:{}}}"
                    }}
                }}
            }}
        }}"#,
            secret_file.display()
        );
        let cat = Catalog::load_from_str(&json, None).unwrap();
        let policy = Policy::new(["custom".to_string()]);

        let req = FakeRequester::new(vec![]);
        req.set_fail_method(Some("_goose/unstable/session/extensions/add".to_string()));
        req.set_fail_error(Some(
            "goose add failed: connection with token secret_key_12345 refused".to_string(),
        ));

        let res = enable(&req, &cat, &policy, "ses_123", &["custom".to_string()], &[])
            .await
            .unwrap();
        assert_eq!(res.results.len(), 1);
        assert!(!res.results[0].ok);
        let err = res.results[0].error.as_deref().unwrap();
        assert!(
            err.contains("[redacted]"),
            "error must contain [redacted]: {err}"
        );
        assert!(
            !err.contains("secret_key_12345"),
            "error must NOT contain the secret: {err}"
        );
    }

    #[tokio::test]
    async fn test_disable_rules() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let initial_exts = vec![
            // figma with marker
            serde_json::json!({
                "extensionKey": "figma",
                "extension": {
                    "type": "mcp",
                    "description": "honk-mcp:figma",
                    "server": { "name": "figma" }
                }
            }),
            // slack WITHOUT marker (user-added or conflict)
            serde_json::json!({
                "extensionKey": "slack",
                "extension": {
                    "type": "mcp",
                    "description": "user-slack",
                    "server": { "name": "slack" }
                }
            }),
            // developer builtin
            serde_json::json!({
                "extensionKey": "developer",
                "extension": {
                    "type": "builtin",
                    "description": "developer platform"
                }
            }),
        ];
        let req = FakeRequester::new(initial_exts);

        // 1. Disable figma (on with marker) -> succeeds
        let res = disable(&req, &cat, &policy, "ses_123", &["figma".to_string()])
            .await
            .unwrap();
        assert_eq!(res.results[0].name, "figma");
        assert!(res.results[0].ok);
        assert_eq!(res.summary, "disabled figma");

        // 2. Disable slack (no marker) -> refused
        let res2 = disable(&req, &cat, &policy, "ses_123", &["slack".to_string()])
            .await
            .unwrap();
        assert_eq!(res2.results[0].name, "slack");
        assert!(!res2.results[0].ok);
        assert_eq!(
            res2.results[0].error.as_deref(),
            Some("not added by honk; refusing to remove")
        );

        // 3. Disable slack-ro (not in goose list) -> already off
        let res3 = disable(&req, &cat, &policy, "ses_123", &["slack-ro".to_string()])
            .await
            .unwrap();
        assert_eq!(res3.results[0].name, "slack-ro");
        assert!(res3.results[0].ok);
        assert_eq!(res3.results[0].note.as_deref(), Some("already off"));

        // Verify developer was NEVER touched
        for (m, p) in req.calls() {
            if m == "_goose/unstable/session/extensions/remove" {
                assert_ne!(p["extensionKey"], "developer");
                assert_ne!(p["extensionKey"], "slack");
            }
        }
    }
}
