mod common;

use common::{Honkd, Viewer, new_session, prompt_params};
use honk_testgoose::{StubGoose, Turn};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn find_python3() -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("which").arg("python3").output()
        && output.status.success()
    {
        let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !p.is_empty() {
            let pb = PathBuf::from(p);
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let candidate = path.join("python3");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

const MCP_FIXTURE_PY: &str = r#"import sys
import json
import os

script_dir = os.path.dirname(os.path.abspath(__file__))
pid = os.getpid()
try:
    with open(os.path.join(script_dir, f"{pid}.pid"), "w") as f:
        f.write(str(pid))
except Exception:
    pass

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    req_id = req.get("id")
    # Ignore notifications (no id)
    if req_id is None:
        continue

    method = req.get("method")
    if method == "initialize":
        pv = req.get("params", {}).get("protocolVersion", "2025-06-18")
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": pv,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "honktest", "version": "1.0.0"}
            }
        }
    elif method == "tools/list":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    {
                        "name": "probe",
                        "description": "Probe test tool",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            }
        }
    elif method == "tools/call":
        tool_name = req.get("params", {}).get("name")
        if tool_name == "probe":
            secret = os.environ.get("HONK_TEST_SECRET")
            slen = len(secret) if secret is not None else -1
            mode = os.environ.get("HONK_TEST_MODE", "unset")
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": f"secret_len={slen} mode={mode}"
                        }
                    ],
                    "isError": False
                }
            }
        else:
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": f"Tool '{tool_name}' not found"
                }
            }
    else:
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32601,
                "message": f"Method '{method}' not found"
            }
        }
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#;

fn assert_bytes_not_in_dir(dir: &Path, forbidden: &[u8]) {
    if !dir.exists() {
        return;
    }
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            assert_bytes_not_in_dir(&path, forbidden);
        } else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap_or_default();
            assert!(
                !bytes.windows(forbidden.len()).any(|w| w == forbidden),
                "forbidden bytes {:?} found in file: {}",
                String::from_utf8_lossy(forbidden),
                path.display()
            );
        }
    }
}

fn assert_bytes_not_in_proc_cmdline(temp_dir: &Path, forbidden: &[u8]) {
    if let Ok(entries) = std::fs::read_dir(temp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("pid")
                && let Ok(pid_str) = std::fs::read_to_string(&path)
            {
                let pid = pid_str.trim();
                let cmdline_path = PathBuf::from(format!("/proc/{pid}/cmdline"));
                if cmdline_path.exists() {
                    let bytes = std::fs::read(&cmdline_path).unwrap_or_default();
                    assert!(
                        !bytes.windows(forbidden.len()).any(|w| w == forbidden),
                        "forbidden bytes {:?} found in /proc/{pid}/cmdline",
                        String::from_utf8_lossy(forbidden)
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mcp_live_e2e() {
    let Some(python3_bin) = find_python3() else {
        eprintln!("skipping test: python3 not on PATH");
        return;
    };

    let Some(mut g) = StubGoose::start_or_skip().await else {
        // StubGoose::start_or_skip already prints why when HONK_GOOSE_BIN is unset or missing
        return;
    };

    // 1 & 2. Setup tempdir with MCP fixture and catalog JSON
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let fixture_path = temp_dir.path().join("fixture.py");
    std::fs::write(&fixture_path, MCP_FIXTURE_PY).expect("write fixture.py");

    let secret_val = "s3cr3t-value";
    let secret_file_content = "  s3cr3t-value\n";
    let secret_path = temp_dir.path().join("secret");
    std::fs::write(&secret_path, secret_file_content).expect("write secret");

    let missing_secret_path = temp_dir.path().join("missing_secret");

    let catalog_content = json!({
        "mcp": {
            "honktest": {
                "type": "local",
                "command": [python3_bin.to_str().unwrap(), fixture_path.to_str().unwrap()],
                "environment": {
                    "HONK_TEST_SECRET": format!("{{file:{}}}", secret_path.display()),
                    "HONK_TEST_MODE": "ro"
                }
            },
            "honktest-w": {
                "type": "local",
                "command": [python3_bin.to_str().unwrap(), fixture_path.to_str().unwrap()]
            },
            "honktest-missing": {
                "type": "local",
                "command": [python3_bin.to_str().unwrap(), fixture_path.to_str().unwrap()],
                "environment": {
                    "HONK_TEST_SECRET": format!("{{file:{}}}", missing_secret_path.display())
                }
            }
        }
    });
    let catalog_path = temp_dir.path().join("opencode.json");
    std::fs::write(&catalog_path, catalog_content.to_string()).expect("write opencode.json");

    // 3. Spawn honkd with --mcp-catalog and --mcp-no-confirm honktest,honktest-missing
    let h = Honkd::spawn(
        &g.url(),
        g.secret_file(),
        &[
            "--mcp-catalog",
            catalog_path.to_str().unwrap(),
            "--mcp-no-confirm",
            "honktest,honktest-missing",
        ],
    )
    .await;

    let mut v = Viewer::connect(&h).await;
    let sid = new_session(&mut v, g.cwd().to_str().unwrap()).await;

    v.call(
        "session/load",
        json!({"sessionId": &sid, "cwd": ".", "mcpServers": []}),
    )
    .await;
    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some(honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(1))
    })
    .await;

    // 4. Assert: _honk/mcp/list shows honktest off
    let list_res = v.call("_honk/mcp/list", json!({ "sessionId": &sid })).await;
    assert!(list_res.get("error").is_none());
    let servers = list_res["result"]["servers"].as_array().unwrap();
    let honktest_srv = servers.iter().find(|s| s["name"] == "honktest").unwrap();
    assert_eq!(honktest_srv["enabled"], false);

    // Enable honktest
    let enable_res = v
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": &sid,
                "names": ["honktest"]
            }),
        )
        .await;
    assert!(
        enable_res.get("error").is_none(),
        "enable honktest failed: {enable_res}"
    );
    assert_eq!(enable_res["result"]["results"][0]["name"], "honktest");
    assert_eq!(enable_res["result"]["results"][0]["ok"], true);

    // Check tools list
    let tools = g.tools_list(&sid).await;
    let probe_tool = tools
        .iter()
        .find(|t| t.ends_with("probe"))
        .unwrap_or_else(|| panic!("expected probe tool in tools_list, found: {tools:?}"))
        .clone();
    // Document the actual tool name prefix used by goose (e.g. "honktest__probe")
    println!("Goose extension tool name: {probe_tool}");
    assert_eq!(probe_tool, "honktest__probe");

    // Script StubModel to call the probe tool
    g.model().push([
        Turn::ToolCall {
            tool: probe_tool.clone(),
            arguments: json!({}),
        },
        Turn::Text(vec!["done".into()]),
    ]);

    let pid = v
        .request(
            "session/prompt",
            prompt_params(&sid, "call probe tool", Some("crid-probe")),
        )
        .await;
    let prompt_resp = v.response(&pid).await;
    assert_eq!(prompt_resp["result"]["stopReason"], "end_turn");

    // Assert tool result text contains secret_len=12 and mode=ro
    let export = g.export(&sid).await;
    let export_str = export.to_string();
    assert!(
        export_str.contains("secret_len=12"),
        "export must contain secret_len=12: {export_str}"
    );
    assert!(
        export_str.contains("mode=ro"),
        "export must contain mode=ro: {export_str}"
    );

    // Assert secret value string does NOT appear in goose's session DB, honkd state dir, or /proc/<pid>/cmdline
    let forbidden = secret_val.as_bytes();
    assert_bytes_not_in_dir(g.root(), forbidden);
    assert_bytes_not_in_dir(h.state_dir.path(), forbidden);
    assert_bytes_not_in_proc_cmdline(temp_dir.path(), forbidden);

    // 5. _honk/mcp/enable honktest-w without confirm -> error -32602; with confirm -> ok
    let refuse_w = v
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": &sid,
                "names": ["honktest-w"]
            }),
        )
        .await;
    assert_eq!(refuse_w["error"]["code"], -32602);
    assert!(
        refuse_w["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'honktest-w' can write; use /mcp enable honktest-w! to confirm")
    );

    let enable_w = v
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": &sid,
                "names": ["honktest-w"],
                "confirm": ["honktest-w"]
            }),
        )
        .await;
    assert!(
        enable_w.get("error").is_none(),
        "enable honktest-w failed: {enable_w}"
    );
    assert_eq!(enable_w["result"]["results"][0]["name"], "honktest-w");
    assert_eq!(enable_w["result"]["results"][0]["ok"], true);

    // 6. Persistence: restart goose and verify after honkd re-activates, honktest still enabled and tool present
    g.restart().await;

    v.wait_for(|f| {
        f.get("method").and_then(Value::as_str) == Some(honk_wire::method::REPLAY_DONE)
            && f.pointer("/params/epoch") == Some(&json!(2))
    })
    .await;

    let list_after_restart = v.call("_honk/mcp/list", json!({ "sessionId": &sid })).await;
    let servers_after = list_after_restart["result"]["servers"].as_array().unwrap();
    let honktest_after = servers_after
        .iter()
        .find(|s| s["name"] == "honktest")
        .unwrap();
    assert_eq!(honktest_after["enabled"], true);

    let tools_after = g.tools_list(&sid).await;
    assert!(
        tools_after.iter().any(|t| t == &probe_tool),
        "tool {probe_tool} must persist after restart, tools: {tools_after:?}"
    );

    // 7. _honk/mcp/disable honktest -> tools/list no longer contains it
    let disable_res = v
        .call(
            "_honk/mcp/disable",
            json!({
                "sessionId": &sid,
                "names": ["honktest"]
            }),
        )
        .await;
    assert!(disable_res.get("error").is_none());
    assert_eq!(disable_res["result"]["results"][0]["name"], "honktest");
    assert_eq!(disable_res["result"]["results"][0]["ok"], true);

    let tools_after_disable = g.tools_list(&sid).await;
    assert!(
        !tools_after_disable.iter().any(|t| t == &probe_tool),
        "tool {probe_tool} must no longer be present after disable: {tools_after_disable:?}"
    );

    let list_after_disable = v.call("_honk/mcp/list", json!({ "sessionId": &sid })).await;
    let srv_disabled = list_after_disable["result"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "honktest")
        .unwrap();
    assert_eq!(srv_disabled["enabled"], false);

    // 8. Catalog entry with missing file -> per-name failure mentioning "cannot read" or exit status
    let enable_missing = v
        .call(
            "_honk/mcp/enable",
            json!({
                "sessionId": &sid,
                "names": ["honktest-missing"]
            }),
        )
        .await;
    assert!(enable_missing.get("error").is_none());
    let missing_res = &enable_missing["result"]["results"][0];
    assert_eq!(missing_res["name"], "honktest-missing");
    assert_eq!(missing_res["ok"], false);
    let err_str = missing_res["error"]
        .as_str()
        .expect("error string in per-name failure");
    assert!(
        err_str.contains("cannot read"),
        "error must mention 'cannot read', got: {err_str}"
    );

    let list_after_missing = v.call("_honk/mcp/list", json!({ "sessionId": &sid })).await;
    let srv_missing = list_after_missing["result"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "honktest-missing")
        .unwrap();
    assert_eq!(srv_missing["enabled"], false);
}
