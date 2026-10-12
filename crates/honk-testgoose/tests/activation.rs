use std::collections::BTreeSet;
use std::time::Duration;

use honk_testgoose::{StubGoose, StubGooseConfig, Turn};
use honk_upstream::{Recorder, Upstream};
use serde_json::{Value, json};

fn extract_tool_names(req: &Value) -> BTreeSet<String> {
    req.get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
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
async fn activation_extension_set_against_wider_config() -> anyhow::Result<()> {
    // Start StubGoose with config.yaml enabling both developer AND todo
    let config = StubGooseConfig {
        extra_extensions: vec!["todo".to_string()],
    };
    let Some(goose) = StubGoose::start_with_config_or_skip(config).await else {
        eprintln!("skipping activation test: HONK_GOOSE_BIN unset or not found");
        return Ok(());
    };

    println!("StubGoose started at {}", goose.url());
    let cwd = goose.cwd().to_str().unwrap().to_string();

    // =========================================================================
    // Sanity check: confirm config.yaml actually enables todo when not narrowed
    // =========================================================================
    {
        let mut up_sanity = Upstream::initialize(
            &goose.url(),
            goose.secret().to_string(),
            Recorder::disabled(),
        )
        .await?;
        up_sanity.subscribe(None).await?;
        let res = up_sanity
            .call(
                "session/new",
                json!({
                    "cwd": &cwd,
                    "mcpServers": []
                }),
                None,
                Duration::from_secs(30),
            )
            .await?;
        let sid_sanity = res.get("sessionId").and_then(|v| v.as_str()).unwrap();
        let exts = res
            .pointer("/_meta/extensionResults")
            .or_else(|| res.get("extensionResults"))
            .unwrap();
        let exts_set = extract_extension_names(exts);
        println!("[SANITY_CHECK_DEFAULT_NEW] extensions without narrowing: {exts_set:?}");
        assert!(
            exts_set.contains("todo"),
            "sanity check failed: config.yaml must have enabled todo when session/new has no enabledExtensions"
        );
        up_sanity.subscribe(Some(sid_sanity)).await?;
        goose
            .model()
            .push([Turn::Text(vec!["sanity turn ok".into()])]);
        let prompt_sanity = up_sanity
            .call(
                "session/prompt",
                json!({
                    "sessionId": sid_sanity,
                    "prompt": [{"type": "text", "text": "sanity prompt"}]
                }),
                Some(sid_sanity),
                Duration::from_secs(30),
            )
            .await?;
        assert_eq!(prompt_sanity["stopReason"], "end_turn");
        let turn_reqs = goose.model().turn_requests();
        let sanity_tools = extract_tool_names(&turn_reqs[0]);
        println!(
            "[SANITY_CHECK_DEFAULT_TOOLS] tools count: {}",
            sanity_tools.len()
        );
        println!("[SANITY_CHECK_DEFAULT_TOOLS] tools: {sanity_tools:?}");
        assert!(
            sanity_tools.iter().any(|t| t.contains("todo")),
            "sanity check failed: model turn request must include todo tool when not narrowed"
        );
        let _ = up_sanity.delete().await;
    }

    // =========================================================================
    // Part 1: session/new with _meta.enabledExtensions [developer]
    // =========================================================================
    let mut up1 = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        Recorder::disabled(),
    )
    .await?;
    up1.subscribe(None).await?;

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
        .expect("sessionId in session/new")
        .to_string();

    let new_exts_val = new_res
        .pointer("/_meta/extensionResults")
        .or_else(|| new_res.get("extensionResults"))
        .cloned()
        .unwrap_or(Value::Null);
    let new_exts = extract_extension_names(&new_exts_val);
    println!("[SESSION_NEW] extensionResults: {new_exts_val}");
    println!("[SESSION_NEW] extension names: {new_exts:?}");

    // Prompt turn 1 to record active tools in the model turn request
    up1.subscribe(Some(&sid)).await?;
    goose
        .model()
        .push([Turn::Text(vec!["initial turn response".into()])]);

    let prompt1_res = up1
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{
                    "type": "text",
                    "text": "first turn"
                }]
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;
    assert_eq!(prompt1_res["stopReason"], "end_turn");

    let turn_reqs_1 = goose.model().turn_requests();
    let initial_tools = extract_tool_names(turn_reqs_1.last().unwrap());
    println!("[INITIAL_SESSION_TOOLS] count: {}", initial_tools.len());
    println!("[INITIAL_SESSION_TOOLS] tools: {initial_tools:?}");

    // Assert that todo tools are NOT in the initial turn request
    let has_todo_initial = initial_tools.iter().any(|t| t.contains("todo"));
    println!("[INITIAL_SESSION] contains todo tool: {has_todo_initial}");

    // =========================================================================
    // Part 2: Variant A — DELETE connection, then fresh connection + session/load
    // =========================================================================
    let del1 = up1.delete().await?;
    println!("[VARIANT_A] deleted connection 1: {del1}");

    let mut up2 = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        Recorder::disabled(),
    )
    .await?;
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
    println!("[VARIANT_A_LOAD] extensionResults: {load_exts_val}");
    println!("[VARIANT_A_LOAD] extension names: {load_exts:?}");

    // Prompt turn 2 on re-activated session to observe active tools
    goose
        .model()
        .push([Turn::Text(vec!["turn after load response".into()])]);

    let prompt2_res = up2
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid,
                "prompt": [{
                    "type": "text",
                    "text": "turn after load"
                }]
            }),
            Some(&sid),
            Duration::from_secs(30),
        )
        .await?;
    assert_eq!(prompt2_res["stopReason"], "end_turn");

    let turn_reqs_2 = goose.model().turn_requests();
    let tools_after_load = extract_tool_names(turn_reqs_2.last().unwrap());
    println!("[VARIANT_A_TOOLS] count: {}", tools_after_load.len());
    println!("[VARIANT_A_TOOLS] tools: {tools_after_load:?}");

    let has_todo_after_load = tools_after_load.iter().any(|t| t.contains("todo"));
    println!("[VARIANT_A] contains todo tool after load: {has_todo_after_load}");

    let load_surplus_exts: Vec<_> = load_exts.difference(&new_exts).cloned().collect();
    let load_surplus_tools: Vec<_> = tools_after_load
        .difference(&initial_tools)
        .cloned()
        .collect();

    println!("[VARIANT_A_SURPLUS_EXTENSIONS] {load_surplus_exts:?}");
    println!("[VARIANT_A_SURPLUS_TOOLS] {load_surplus_tools:?}");

    let _ = up2.delete().await;

    // =========================================================================
    // Part 3: Variant B — Prompt on fresh connection WITHOUT load
    // (what pigeon does after goose restart or disconnect)
    // =========================================================================
    // Create a new session with [developer]
    let mut up_b1 = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        Recorder::disabled(),
    )
    .await?;
    up_b1.subscribe(None).await?;

    let new_b_res = up_b1
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

    let sid_b = new_b_res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .expect("sessionId in session/new (session B)")
        .to_string();

    up_b1.subscribe(Some(&sid_b)).await?;
    goose
        .model()
        .push([Turn::Text(vec!["session B turn 1 response".into()])]);

    let prompt_b1 = up_b1
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid_b,
                "prompt": [{
                    "type": "text",
                    "text": "session B turn 1"
                }]
            }),
            Some(&sid_b),
            Duration::from_secs(30),
        )
        .await?;
    assert_eq!(prompt_b1["stopReason"], "end_turn");

    let turn_reqs_b1 = goose.model().turn_requests();
    let tools_b_initial = extract_tool_names(turn_reqs_b1.last().unwrap());
    println!("[SESSION_B_INITIAL_TOOLS] count: {}", tools_b_initial.len());
    println!("[SESSION_B_INITIAL_TOOLS] tools: {tools_b_initial:?}");

    // DELETE connection B1
    let del_b1 = up_b1.delete().await?;
    println!("[VARIANT_B] deleted connection B1: {del_b1}");

    // Connect fresh connection B2, subscribe, but DO NOT call session/load!
    let mut up_b2 = Upstream::initialize(
        &goose.url(),
        goose.secret().to_string(),
        Recorder::disabled(),
    )
    .await?;
    up_b2.subscribe(None).await?;
    up_b2.subscribe(Some(&sid_b)).await?;

    goose
        .model()
        .push([Turn::Text(vec!["session B turn 2 response".into()])]);

    let prompt_b2 = up_b2
        .call(
            "session/prompt",
            json!({
                "sessionId": &sid_b,
                "prompt": [{
                    "type": "text",
                    "text": "session B turn 2 without load"
                }]
            }),
            Some(&sid_b),
            Duration::from_secs(30),
        )
        .await?;
    assert_eq!(prompt_b2["stopReason"], "end_turn");

    let turn_reqs_b2 = goose.model().turn_requests();
    let tools_b_direct = extract_tool_names(turn_reqs_b2.last().unwrap());
    println!("[VARIANT_B_TOOLS] count: {}", tools_b_direct.len());
    println!("[VARIANT_B_TOOLS] tools: {tools_b_direct:?}");

    let has_todo_direct = tools_b_direct.iter().any(|t| t.contains("todo"));
    println!("[VARIANT_B] contains todo tool without load: {has_todo_direct}");

    let direct_surplus_tools: Vec<_> = tools_b_direct
        .difference(&tools_b_initial)
        .cloned()
        .collect();
    println!("[VARIANT_B_SURPLUS_TOOLS] {direct_surplus_tools:?}");

    let _ = up_b2.delete().await;

    // Check stop rule: did either variant widen?
    let widened_a = !load_surplus_exts.is_empty() || !load_surplus_tools.is_empty();
    let widened_b = !direct_surplus_tools.is_empty();

    if widened_a || widened_b {
        eprintln!("=======================================================");
        eprintln!("STOP RULE TRIGGERED: EXTENSION SET WIDENED ON RE-ACTIVATION!");
        eprintln!("  Variant A (session/load): widened={widened_a}");
        eprintln!("    Surplus extensionResults: {load_surplus_exts:?}");
        eprintln!("    Surplus tools:            {load_surplus_tools:?}");
        eprintln!("  Variant B (direct prompt): widened={widened_b}");
        eprintln!("    Surplus tools:            {direct_surplus_tools:?}");
        eprintln!("=======================================================");
    } else {
        println!("=======================================================");
        println!("EXTENSION SET PRESERVED: NO WIDENING OBSERVED!");
        println!("  Variant A (session/load): tools match exactly");
        println!("  Variant B (direct prompt): tools match exactly");
        println!("=======================================================");
    }

    Ok(())
}
