use std::collections::BTreeSet;
use std::time::Duration;

use honk_upstream::{Recorder, Upstream};
use serde_json::{Value, json};

fn probe_url() -> String {
    std::env::var("HONK_GOOSE_URL").unwrap_or_else(|_| "http://127.0.0.1:4181/acp".into())
}

fn probe_secret() -> Option<String> {
    match std::env::var("HONK_GOOSE_SECRET") {
        Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => {
            if let Ok(home) = std::env::var("HOME") {
                let path =
                    std::path::PathBuf::from(home).join(".local/state/honk-sandbox-patched/secret");
                if let Ok(s) = std::fs::read_to_string(&path)
                    && !s.trim().is_empty()
                {
                    return Some(s.trim().to_string());
                }
            }
            None
        }
    }
}

fn extract_extension_names(val: &Value) -> BTreeSet<String> {
    if let Some(arr) = val.as_array() {
        arr.iter()
            .filter_map(|item| {
                item.get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    } else {
        BTreeSet::new()
    }
}

#[tokio::test]
#[ignore]
async fn live_activation_extension_set() -> anyhow::Result<()> {
    let url = probe_url();
    let Some(secret) = probe_secret() else {
        eprintln!("skipping live_activation: HONK_GOOSE_SECRET unset and secret file unreadable");
        return Ok(());
    };

    println!("Connecting to upstream at {url} for live_activation probe");

    // 1. Connection A: session/new with _meta.enabledExtensions = [developer]
    let mut up1 = Upstream::initialize(&url, secret.clone(), Recorder::disabled()).await?;
    up1.subscribe(None).await?;

    let temp_dir = tempfile::tempdir()?;
    let cwd = temp_dir.path().to_str().unwrap().to_string();

    let new_res = up1
        .call(
            "session/new",
            json!({
                "cwd": &cwd,
                "mcpServers": [],
                "_meta": {
                    "enabledExtensions": [{
                        "type": "platform",
                        "name": "developer"
                    }]
                }
            }),
            None,
            Duration::from_secs(30),
        )
        .await?;

    let sid = new_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("session/new reply missing sessionId")
        .to_string();

    let new_exts_val = new_res
        .pointer("/_meta/extensionResults")
        .or_else(|| new_res.get("extensionResults"))
        .cloned()
        .unwrap_or(Value::Null);

    let new_exts = extract_extension_names(&new_exts_val);
    println!("session/new ({sid}) extensionResults: {new_exts_val}");
    println!("session/new parsed extension names: {new_exts:?}");

    // 2. DELETE connection A
    let del_status = up1.delete().await?;
    println!("Connection A deleted with status: {del_status}");

    // 3. Connection B: fresh connection, session/load with stored cwd and mcpServers: []
    let mut up2 = Upstream::initialize(&url, secret.clone(), Recorder::disabled()).await?;
    up2.subscribe(None).await?;
    up2.subscribe(Some(&sid)).await?;

    let load_res = up2
        .call(
            "session/load",
            json!({
                "sessionId": &sid,
                "cwd": &cwd,
                "mcpServers": []
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;

    let load_exts_val = load_res
        .pointer("/_meta/extensionResults")
        .or_else(|| load_res.get("extensionResults"))
        .cloned()
        .unwrap_or(Value::Null);

    let load_exts = extract_extension_names(&load_exts_val);
    println!("session/load ({sid}) extensionResults: {load_exts_val}");
    println!("session/load parsed extension names: {load_exts:?}");

    // Clean up connection B
    let _ = up2.delete().await;

    // Check if the set widened
    let surplus: Vec<_> = load_exts.difference(&new_exts).cloned().collect();
    let missing: Vec<_> = new_exts.difference(&load_exts).cloned().collect();

    println!("[ACTIVATION_EXTENSION_COMPARISON]");
    println!("  session/new:  {new_exts:?}");
    println!("  session/load: {load_exts:?}");
    println!("  surplus (widened): {surplus:?}");
    println!("  missing:           {missing:?}");

    if !surplus.is_empty() {
        eprintln!(
            "STOP RULE TRIGGERED: extension set widened on session/load! Surplus extensions: {surplus:?}"
        );
        panic!(
            "STOP RULE TRIGGERED: extension set widened on session/load! Surplus extensions: {surplus:?}"
        );
    }

    Ok(())
}
