//! MCP session operations: list, enable, and disable.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::future::Future;

use crate::mcp::catalog::{Catalog, CatalogEntry, EntrySource, Transport, marker};
use crate::mcp::probe::RemoteProbe;

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
    /// Where the entry came from: `user`, `project:<file>` or `builtin`.
    /// Absent from replies of older honkd versions.
    #[serde(default)]
    pub source: String,
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

/// The `-ro` / non-`-ro` twin of `name`, when the catalog has one from the
/// same source. A project entry never pairs with a user entry (or the reverse),
/// so a repo cannot make enabling its server remove one the user turned on.
fn find_sibling(name: &str, catalog: &Catalog) -> Option<String> {
    let entry = catalog.get(name)?;
    let candidate = crate::mcp::catalog::sibling_name(name);
    let other = catalog.get(&candidate)?;
    (std::mem::discriminant(&entry.source) == std::mem::discriminant(&other.source))
        .then_some(candidate)
}

/// Whether enabling `entry` needs the person's `name!` confirmation.
///
/// A project entry is judged by what it is, never by its name: only a
/// loopback remote entry is trusted, so a repo cannot borrow a name the user
/// listed in `--mcp-no-confirm`. Everything else follows the configured policy.
fn needs_confirm(entry: &CatalogEntry, policy: &Policy) -> bool {
    match entry.source {
        EntrySource::Project(_) => !entry.auto_eligible(),
        _ => !policy.no_confirm.contains(&entry.name),
    }
}

fn project_refusal_hint(name: &str) -> String {
    format!(
        "'{name}' comes from this project's config and is not a loopback server; only a person can enable it, with /mcp enable {name}!"
    )
}

fn project_reason_hint(name: &str) -> String {
    format!(
        "'{name}' comes from this project's config; to enable it, call mcp_enable with \"{name}\" and a reason saying what you will do with it"
    )
}

fn confirm_refusal_hint(name: &str, catalog: &Catalog, is_agent: bool) -> String {
    let mut msg = if is_agent {
        format!(
            "'{name}' can write; to confirm, call mcp_enable with \"{name}!\" and a reason saying what you will do with it"
        )
    } else {
        format!("'{name}' can write; use /mcp enable {name}! to confirm")
    };
    if let Some(sibling) = find_sibling(name, catalog)
        && sibling.ends_with("-ro")
    {
        if is_agent {
            msg.push_str(&format!(" (or \"{sibling}\" for read-only)"));
        } else {
            msg.push_str(&format!(" (or /mcp enable {sibling} for read-only)"));
        }
    }
    msg
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
        let expected_marker = marker(name);

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
        let confirm = needs_confirm(entry, policy);
        let sibling = find_sibling(name, catalog);

        servers.push(ServerInfo {
            name: name.clone(),
            enabled,
            supported,
            reason,
            confirm,
            source: entry.source.label(),
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

/// Whether the caller must justify a confirmed enable: an agent must give a
/// reason, a person typing `/mcp enable name!` need not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonPolicy<'a> {
    NotRequired,
    Required(Option<&'a str>),
}

/// Enables the specified MCP servers in the given goose session.
///
/// Validates all server names before executing any requests. If any server name requires
/// confirmation and `confirm` does not list it, returns an invalid params error guiding
/// the user. An agent cannot enable a project-declared server that is not trusted
/// ([`CatalogEntry::auto_eligible`]); only a person can.
///
/// If an exclusive sibling (e.g. `slack-ro` when enabling `slack`, or vice versa) is currently
/// enabled with the marker, it is removed before adding the new server.
///
/// A remote (http) server is probed first; one that is down, or that wants OAuth,
/// is reported as a failed name and left alone (see [`crate::mcp::probe`]).
#[allow(clippy::too_many_arguments)]
pub async fn enable_with_reason(
    req: &impl GooseRequester,
    probe: &impl RemoteProbe,
    catalog: &Catalog,
    policy: &Policy,
    session_id: &str,
    names: &[String],
    confirm: &[String],
    reason_policy: ReasonPolicy<'_>,
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

        let is_agent = matches!(reason_policy, ReasonPolicy::Required(_));
        let is_project = matches!(entry.source, EntrySource::Project(_));
        if is_agent && is_project && !entry.auto_eligible() {
            return Err(OpError::invalid_params(project_refusal_hint(name)));
        }

        let confirm_needed = needs_confirm(entry, policy);
        if confirm_needed && !confirm.iter().any(|c| c == name) {
            return Err(OpError::invalid_params(confirm_refusal_hint(
                name, catalog, is_agent,
            )));
        }

        // An agent justifies every `!` and every project-declared server, even
        // a trusted loopback one: a repo chose it, not the user.
        if let ReasonPolicy::Required(reason_opt) = reason_policy
            && (confirm_needed || is_project)
        {
            let is_empty = reason_opt.map(|r| r.trim().is_empty()).unwrap_or(true);
            if is_empty {
                return Err(OpError::invalid_params(if confirm_needed {
                    confirm_refusal_hint(name, catalog, true)
                } else {
                    project_reason_hint(name)
                }));
            }
            if let Some(r) = reason_opt
                && r.chars().count() > 500
            {
                return Err(OpError::invalid_params(
                    "reason too long (max 500 characters)",
                ));
            }
        }
    }

    let mut goose_exts = fetch_goose_extensions(req, session_id).await?;

    let mut results = Vec::new();
    for name in &unique_names {
        let expected_marker = marker(name);
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

        // A remote server that is down or wants OAuth must not reach goose, and
        // must not cost the session a working sibling.
        let spec = catalog.get(name).unwrap().supported.as_ref().unwrap();
        if let Transport::Http { url } = &spec.transport
            && let Err(reason) = probe.probe(url).await
        {
            results.push(NameResult {
                name: name.clone(),
                ok: false,
                error: Some(reason),
                note: None,
                replaced: None,
                changed: false,
            });
            continue;
        }

        // Check if sibling is currently enabled WITH marker
        let mut replaced = None;
        if let Some(sibling) = find_sibling(name, catalog) {
            let sibling_marker = marker(&sibling);
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
/// Only removes extensions carrying the `honk-mcp:<name>` marker, whether or not the
/// catalog still has the name. Builtin and platform
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

    let mut goose_exts = fetch_goose_extensions(req, session_id).await?;

    // A name the catalog no longer has (its project file changed, say) is still
    // known when goose holds an extension honk marked with that name.
    for name in &unique_names {
        let marked = goose_exts
            .iter()
            .any(|e| &e.key == name && e.description.as_deref() == Some(&marker(name)));
        if catalog.get(name).is_none() && !marked {
            return Err(OpError::invalid_params(format!(
                "unknown MCP server '{name}'; /mcp lists them"
            )));
        }
    }

    let mut results = Vec::new();
    for name in &unique_names {
        let expected_marker = marker(name);
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

    /// A probe that answers from a table: a url in `down` fails with its reason.
    #[derive(Default)]
    struct FakeProbe {
        down: Mutex<std::collections::HashMap<String, String>>,
        probed: Mutex<Vec<String>>,
    }

    impl FakeProbe {
        fn failing(url: &str, reason: &str) -> Self {
            let p = Self::default();
            p.down.lock().unwrap().insert(url.into(), reason.into());
            p
        }

        fn probed(&self) -> Vec<String> {
            self.probed.lock().unwrap().clone()
        }
    }

    impl RemoteProbe for FakeProbe {
        async fn probe(&self, url: &str) -> Result<(), String> {
            self.probed.lock().unwrap().push(url.to_string());
            match self.down.lock().unwrap().get(url) {
                Some(reason) => Err(reason.clone()),
                None => Ok(()),
            }
        }
    }

    async fn enable(
        req: &impl GooseRequester,
        catalog: &Catalog,
        policy: &Policy,
        session_id: &str,
        names: &[String],
        confirm: &[String],
    ) -> Result<OpResult, OpError> {
        enable_with_reason(
            req,
            &FakeProbe::default(),
            catalog,
            policy,
            session_id,
            names,
            confirm,
            ReasonPolicy::NotRequired,
        )
        .await
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
            "'slack' can write; use /mcp enable slack! to confirm (or /mcp enable slack-ro for read-only)"
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
            "'pagerduty' can write; use /mcp enable pagerduty! to confirm"
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
            "'figma' can write; use /mcp enable figma! to confirm"
        );
    }

    #[tokio::test]
    async fn test_agent_enable_reason_requirements() {
        let cat = sample_catalog();
        let policy = sample_policy();
        let req = FakeRequester::new(vec![]);

        // 1. Without confirm, hint error is returned first (even if reason is missing)
        let err1 = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &[],
            ReasonPolicy::Required(None),
        )
        .await
        .unwrap_err();
        assert_eq!(err1.code, -32602);
        assert_eq!(
            err1.message,
            "'slack' can write; to confirm, call mcp_enable with \"slack!\" and a reason saying what you will do with it (or \"slack-ro\" for read-only)"
        );

        // 2. With confirm but no reason -> reason error
        let err2 = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &["slack".to_string()],
            ReasonPolicy::Required(None),
        )
        .await
        .unwrap_err();
        assert_eq!(err2.code, -32602);
        assert_eq!(
            err2.message,
            "'slack' can write; to confirm, call mcp_enable with \"slack!\" and a reason saying what you will do with it (or \"slack-ro\" for read-only)"
        );

        // 3. With confirm but whitespace-only reason -> reason error
        let err3 = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &["slack".to_string()],
            ReasonPolicy::Required(Some("   \t  ")),
        )
        .await
        .unwrap_err();
        assert_eq!(err3.code, -32602);
        assert_eq!(
            err3.message,
            "'slack' can write; to confirm, call mcp_enable with \"slack!\" and a reason saying what you will do with it (or \"slack-ro\" for read-only)"
        );

        // 4. With confirm and valid reason -> OK
        let res_ok = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["slack".to_string()],
            &["slack".to_string()],
            ReasonPolicy::Required(Some("investigate customer ticket")),
        )
        .await
        .unwrap();
        assert!(res_ok.results[0].ok);

        // 5. No-confirm server (slack-ro) needs neither confirm nor reason
        let res_no_confirm = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["slack-ro".to_string()],
            &[],
            ReasonPolicy::Required(None),
        )
        .await
        .unwrap();
        assert!(res_no_confirm.results[0].ok);

        // 6. Server without sibling (pagerduty) returns hint without "(or ...)"
        let err_pd = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_123",
            &["pagerduty".to_string()],
            &[],
            ReasonPolicy::Required(None),
        )
        .await
        .unwrap_err();
        assert_eq!(err_pd.code, -32602);
        assert_eq!(
            err_pd.message,
            "'pagerduty' can write; to confirm, call mcp_enable with \"pagerduty!\" and a reason saying what you will do with it"
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

    #[tokio::test]
    async fn test_enable_swarm_builtin_without_confirm() {
        let mut cat = Catalog::default();
        let swarm_cfg = crate::mcp::SwarmMcpConfig {
            bin: std::path::PathBuf::from("/etc/profiles/per-user/dev/bin/honk"),
            pigeon_url: "http://127.0.0.1:4731".into(),
            pigeon_token_file: None,
            honk_url: "http://127.0.0.1:4280/acp".into(),
            honk_secret_file: std::path::PathBuf::from("/tmp/state/secret"),
        };
        cat.merge_swarm_entry(&swarm_cfg, "ses_goose_1");

        let mut policy = Policy::default();
        policy.no_confirm.insert("swarm".to_string());

        let req = FakeRequester::new(vec![]);

        // Confirm array is empty, but swarm is implicitly no-confirm -> succeeds!
        let res = enable(
            &req,
            &cat,
            &policy,
            "ses_goose_1",
            &["swarm".to_string()],
            &[],
        )
        .await
        .unwrap();

        assert_eq!(res.results.len(), 1);
        assert_eq!(res.results[0].name, "swarm");
        assert!(res.results[0].ok);
        assert_eq!(res.summary, "enabled swarm");

        // Verify extensions/add call
        let calls = req.calls();
        let add_call = calls
            .iter()
            .find(|(m, _)| m == "_goose/unstable/session/extensions/add")
            .expect("extensions/add call");
        let ext = &add_call.1["extension"];
        assert_eq!(ext["type"], "mcp");
        assert_eq!(ext["description"], "honk-mcp:swarm");
        assert_eq!(
            ext["server"]["command"],
            "/etc/profiles/per-user/dev/bin/honk"
        );
        let args = ext["server"]["args"].as_array().unwrap();
        assert_eq!(args[0], "swarm-mcp");
        assert_eq!(args[1], "--session");
        assert_eq!(args[2], "ses_goose_1");
    }

    fn project_catalog(json: &str) -> Catalog {
        let mut cat = Catalog::load_from_str(json, None).unwrap();
        for e in &mut cat.entries {
            e.source = EntrySource::Project("opencode.json".into());
        }
        cat
    }

    const LOOPBACK_TEC: &str =
        r#"{"mcp":{"tec":{"type":"remote","url":"http://127.0.0.1:4006/mcp"}}}"#;
    const REMOTE_TEC: &str =
        r#"{"mcp":{"tec":{"type":"remote","url":"https://tec.example.com/mcp"}}}"#;

    fn add_calls(req: &FakeRequester) -> Vec<Value> {
        req.calls()
            .into_iter()
            .filter(|(m, _)| m == "_goose/unstable/session/extensions/add")
            .map(|(_, p)| p)
            .collect()
    }

    #[tokio::test]
    async fn test_remote_enable_adds_http_extension_after_probe() {
        let cat = Catalog::load_from_str(LOOPBACK_TEC, None).unwrap();
        let policy = Policy::new(["tec".to_string()]);
        let req = FakeRequester::new(vec![]);
        let probe = FakeProbe::default();

        let res = enable_with_reason(
            &req,
            &probe,
            &cat,
            &policy,
            "ses_1",
            &["tec".to_string()],
            &[],
            ReasonPolicy::NotRequired,
        )
        .await
        .unwrap();
        assert!(res.results[0].ok, "{:?}", res.results[0]);
        assert_eq!(probe.probed(), vec!["http://127.0.0.1:4006/mcp"]);
        let adds = add_calls(&req);
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0]["extension"]["server"]["type"], "http");
        assert_eq!(adds[0]["extension"]["description"], "honk-mcp:tec");
    }

    #[tokio::test]
    async fn test_probe_failure_is_a_failed_name_not_a_failed_op() {
        let json = r#"{"mcp":{
            "remote-a":{"type":"remote","url":"http://127.0.0.1:1/mcp"},
            "local-b":{"type":"local","command":["b-mcp"]}
        }}"#;
        let cat = Catalog::load_from_str(json, None).unwrap();
        let policy = Policy::new(["remote-a".to_string(), "local-b".to_string()]);
        let req = FakeRequester::new(vec![]);
        let probe = FakeProbe::failing("http://127.0.0.1:1/mcp", "not reachable at x: refused");

        let res = enable_with_reason(
            &req,
            &probe,
            &cat,
            &policy,
            "ses_1",
            &["remote-a".to_string(), "local-b".to_string()],
            &[],
            ReasonPolicy::NotRequired,
        )
        .await
        .unwrap();

        assert!(!res.results[0].ok);
        assert_eq!(
            res.results[0].error.as_deref(),
            Some("not reachable at x: refused")
        );
        assert!(!res.results[0].changed);
        assert!(res.results[1].ok, "the other name still lands");
        // Only the remote one was probed, and only the local one was added.
        assert_eq!(probe.probed(), vec!["http://127.0.0.1:1/mcp"]);
        let adds = add_calls(&req);
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0]["extension"]["server"]["name"], "local-b");
    }

    #[tokio::test]
    async fn test_already_on_remote_is_not_probed() {
        let cat = Catalog::load_from_str(LOOPBACK_TEC, None).unwrap();
        let policy = Policy::new(["tec".to_string()]);
        let req = FakeRequester::new(vec![serde_json::json!({
            "extensionKey": "tec",
            "extension": {"type": "mcp", "description": "honk-mcp:tec", "server": {"name": "tec"}}
        })]);
        let probe = FakeProbe::failing("http://127.0.0.1:4006/mcp", "down");
        let res = enable_with_reason(
            &req,
            &probe,
            &cat,
            &policy,
            "ses_1",
            &["tec".to_string()],
            &[],
            ReasonPolicy::NotRequired,
        )
        .await
        .unwrap();
        assert!(res.results[0].ok);
        assert_eq!(res.results[0].note.as_deref(), Some("already on"));
        assert!(probe.probed().is_empty());
    }

    async fn agent_enable(
        req: &FakeRequester,
        cat: &Catalog,
        name: &str,
        reason: Option<&str>,
    ) -> Result<OpResult, OpError> {
        enable_with_reason(
            req,
            &FakeProbe::default(),
            cat,
            &Policy::default(),
            "ses_1",
            &[name.to_string()],
            &[],
            ReasonPolicy::Required(reason),
        )
        .await
    }

    #[tokio::test]
    async fn test_loopback_project_entry_needs_no_confirm_but_an_agent_needs_a_reason() {
        let cat = project_catalog(LOOPBACK_TEC);
        let policy = Policy::default();

        // A person: no confirm, no reason.
        let req = FakeRequester::new(vec![]);
        let res = enable(&req, &cat, &policy, "ses_1", &["tec".to_string()], &[])
            .await
            .unwrap();
        assert!(res.results[0].ok);

        // An agent: no confirm, but a reason, and nothing happens without one.
        for bad in [None, Some(""), Some("  \t ")] {
            let req = FakeRequester::new(vec![]);
            let err = agent_enable(&req, &cat, "tec", bad).await.unwrap_err();
            assert_eq!(err.code, -32602);
            assert!(
                err.message.contains("reason") && err.message.contains("mcp_enable"),
                "{}",
                err.message
            );
            assert!(req.calls().is_empty());
        }
        let req = FakeRequester::new(vec![]);
        let err = agent_enable(&req, &cat, "tec", Some(&"x".repeat(501)))
            .await
            .unwrap_err();
        assert_eq!(err.message, "reason too long (max 500 characters)");
        assert!(req.calls().is_empty());

        let res = agent_enable(&req, &cat, "tec", Some(&"x".repeat(500)))
            .await
            .unwrap();
        assert!(res.results[0].ok);

        // The list still says no confirm is needed.
        let listed = list(&req, &cat, &policy, "ses_1").await.unwrap();
        assert!(!listed.servers[0].confirm);
        assert_eq!(listed.servers[0].source, "project:opencode.json");
    }

    #[tokio::test]
    async fn test_agent_reason_is_not_required_for_user_entries_without_confirm() {
        let cat = Catalog::load_from_str(LOOPBACK_TEC, None).unwrap();
        let policy = Policy::new(["tec".to_string()]);
        let req = FakeRequester::new(vec![]);
        let res = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_1",
            &["tec".to_string()],
            &[],
            ReasonPolicy::Required(None),
        )
        .await
        .unwrap();
        assert!(res.results[0].ok);
    }

    /// A user `slack` and a project `slack-ro` (both sources hold both halves of a pair).
    fn mixed_source_catalog() -> Catalog {
        let mut cat = Catalog::load_from_str(
            r#"{"mcp":{"slack":{"type":"local","command":["slack-mcp"]}}}"#,
            None,
        )
        .unwrap();
        let mut ro = Catalog::load_from_str(
            r#"{"mcp":{"slack-ro":{"type":"remote","url":"http://127.0.0.1:4006/mcp"}}}"#,
            None,
        )
        .unwrap();
        ro.entries[0].source = EntrySource::Project("opencode.json".into());
        cat.entries.extend(ro.entries);
        cat.entries.sort_by(|a, b| a.name.cmp(&b.name));
        cat
    }

    #[tokio::test]
    async fn test_siblings_only_pair_within_one_source() {
        let cat = mixed_source_catalog();
        let policy = Policy::new(["slack".to_string()]);
        assert_eq!(find_sibling("slack", &cat), None);
        assert_eq!(find_sibling("slack-ro", &cat), None);

        // slack is on; enabling the project's slack-ro must not remove it.
        let req = FakeRequester::new(vec![serde_json::json!({
            "extensionKey": "slack",
            "extension": {"type": "mcp", "description": "honk-mcp:slack", "server": {"name": "slack"}}
        })]);
        let res = enable(&req, &cat, &policy, "ses_1", &["slack-ro".to_string()], &[])
            .await
            .unwrap();
        assert!(res.results[0].ok, "{:?}", res.results[0]);
        assert_eq!(res.results[0].replaced, None);
        assert!(
            req.calls()
                .iter()
                .all(|(m, _)| m != "_goose/unstable/session/extensions/remove")
        );
        assert!(res.enabled.contains(&"slack".to_string()));

        // And both can be named in one request.
        let req = FakeRequester::new(vec![]);
        let res = enable(
            &req,
            &cat,
            &policy,
            "ses_1",
            &["slack".to_string(), "slack-ro".to_string()],
            &[],
        )
        .await
        .unwrap();
        assert!(res.results.iter().all(|r| r.ok));

        // The list shows no sibling across sources.
        let listed = list(&req, &cat, &policy, "ses_1").await.unwrap();
        assert!(listed.servers.iter().all(|s| s.sibling.is_none()));
    }

    #[tokio::test]
    async fn test_project_siblings_still_pair_with_each_other() {
        let cat = project_catalog(
            r#"{"mcp":{
              "foo":{"type":"remote","url":"http://127.0.0.1:1/mcp"},
              "foo-ro":{"type":"remote","url":"http://127.0.0.1:2/mcp"}
            }}"#,
        );
        assert_eq!(find_sibling("foo", &cat).as_deref(), Some("foo-ro"));
        assert_eq!(find_sibling("foo-ro", &cat).as_deref(), Some("foo"));
    }

    #[tokio::test]
    async fn test_agent_cannot_enable_untrusted_project_entry() {
        let cat = project_catalog(REMOTE_TEC);
        let policy = Policy::default();
        let req = FakeRequester::new(vec![]);

        // Not even with confirm and a reason.
        let err = enable_with_reason(
            &req,
            &FakeProbe::default(),
            &cat,
            &policy,
            "ses_1",
            &["tec".to_string()],
            &["tec".to_string()],
            ReasonPolicy::Required(Some("the repo asked for it")),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("only a person can enable it"),
            "{}",
            err.message
        );
        assert!(err.message.contains("/mcp enable tec!"), "{}", err.message);
        assert!(req.calls().is_empty());

        // A person without confirm is told how to confirm...
        let err = enable(&req, &cat, &policy, "ses_1", &["tec".to_string()], &[])
            .await
            .unwrap_err();
        assert_eq!(
            err.message,
            "'tec' can write; use /mcp enable tec! to confirm"
        );

        // ...and with confirm it goes through.
        let res = enable(
            &req,
            &cat,
            &policy,
            "ses_1",
            &["tec".to_string()],
            &["tec".to_string()],
        )
        .await
        .unwrap();
        assert!(res.results[0].ok);
        let listed = list(&req, &cat, &policy, "ses_1").await.unwrap();
        assert!(listed.servers[0].confirm);
    }

    #[tokio::test]
    async fn test_project_entry_does_not_inherit_a_trusted_name() {
        // The user trusted a name that the catalog does not hold; a repo defines it.
        let cat = project_catalog(REMOTE_TEC);
        let policy = Policy::new(["tec".to_string()]);
        let req = FakeRequester::new(vec![]);

        let err = enable(&req, &cat, &policy, "ses_1", &["tec".to_string()], &[])
            .await
            .unwrap_err();
        assert!(err.message.contains("/mcp enable tec!"), "{}", err.message);
        assert!(req.calls().is_empty());
    }

    #[tokio::test]
    async fn test_unsupported_project_entry_is_refused() {
        let cat = project_catalog(
            r#"{"mcp":{"tec":{"type":"remote","url":"http://127.0.0.1:4006/mcp","headers":{"X":"y"}}}}"#,
        );
        let req = FakeRequester::new(vec![]);
        let err = enable(
            &req,
            &cat,
            &Policy::default(),
            "ses_1",
            &["tec".to_string()],
            &["tec".to_string()],
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("'tec' is not supported"));
        assert!(req.calls().is_empty());
    }

    #[tokio::test]
    async fn test_disable_by_marker_after_the_entry_left_the_catalog() {
        let cat = Catalog::default();
        let policy = Policy::default();
        let marked = serde_json::json!({
            "extensionKey": "tec",
            "extension": {"type": "mcp", "description": "honk-mcp:tec", "server": {"name": "tec"}}
        });
        let req = FakeRequester::new(vec![marked]);

        let res = disable(&req, &cat, &policy, "ses_1", &["tec".to_string()])
            .await
            .unwrap();
        assert!(res.results[0].ok);
        assert!(res.results[0].changed);
        assert_eq!(res.summary, "disabled tec");
        assert!(req.calls().iter().any(|(m, p)| {
            m == "_goose/unstable/session/extensions/remove" && p["extensionKey"] == "tec"
        }));
    }

    #[tokio::test]
    async fn test_disable_unknown_unless_marked() {
        let cat = Catalog::default();
        let policy = Policy::default();
        let req = FakeRequester::new(vec![
            // Same key, someone else's extension.
            serde_json::json!({
                "extensionKey": "tec",
                "extension": {"type": "mcp", "description": "user-added", "server": {"name": "tec"}}
            }),
            // Marked for a different name.
            serde_json::json!({
                "extensionKey": "other",
                "extension": {"type": "mcp", "description": "honk-mcp:tec", "server": {"name": "other"}}
            }),
        ]);

        for name in ["tec", "other", "nothing"] {
            let err = disable(&req, &cat, &policy, "ses_1", &[name.to_string()])
                .await
                .unwrap_err();
            assert_eq!(err.code, -32602);
            assert_eq!(
                err.message,
                format!("unknown MCP server '{name}'; /mcp lists them")
            );
        }
        assert!(
            req.calls()
                .iter()
                .all(|(m, _)| m != "_goose/unstable/session/extensions/remove")
        );
    }
}
