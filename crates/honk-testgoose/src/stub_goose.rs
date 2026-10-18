use crate::stub_model::StubModel;
use honk_upstream::{Recorder, Upstream};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[derive(Debug, Clone, Default)]
pub struct StubGooseConfig {
    pub extra_extensions: Vec<String>,
}

pub struct StubGoose {
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) pid: u32,
    pub(crate) port: u16,
    pub(crate) secret: String,
    pub(crate) secret_path: PathBuf,
    pub(crate) temp_dir: TempDir,
    pub(crate) root_dir: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) cwd_dir: PathBuf,
    pub(crate) goose_bin: PathBuf,
    pub(crate) model: StubModel,
}

impl StubGoose {
    pub async fn start_or_skip() -> Option<Self> {
        let model = StubModel::start().await;
        Self::start_with_model_or_skip(model).await
    }

    pub async fn start_with_config_or_skip(config: StubGooseConfig) -> Option<Self> {
        let model = StubModel::start().await;
        Self::start_with_model_and_config_or_skip(model, config).await
    }

    pub async fn start_with_model_or_skip(model: StubModel) -> Option<Self> {
        Self::start_with_model_and_config_or_skip(model, StubGooseConfig::default()).await
    }

    pub async fn start_with_model_and_config_or_skip(
        model: StubModel,
        config: StubGooseConfig,
    ) -> Option<Self> {
        let goose_bin = match std::env::var("HONK_GOOSE_BIN") {
            Ok(b) if !b.trim().is_empty() => PathBuf::from(b.trim()),
            _ => {
                eprintln!("skipping stub goose: HONK_GOOSE_BIN unset");
                return None;
            }
        };

        if !goose_bin.exists() {
            eprintln!(
                "skipping stub goose: HONK_GOOSE_BIN does not exist: {}",
                goose_bin.display()
            );
            return None;
        }

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let temp_path = temp_dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| temp_dir.path().to_path_buf());
        let root_dir = temp_path.join("root");
        let home_dir = temp_path.join("home");
        let cwd_dir = temp_path.join("cwd");

        std::fs::create_dir_all(&root_dir).expect("create root_dir");
        std::fs::create_dir_all(&home_dir).expect("create home_dir");
        std::fs::create_dir_all(&cwd_dir).expect("create cwd_dir");

        let config_dir = root_dir.join("config");
        std::fs::create_dir_all(&config_dir).expect("create config_dir");
        let mut config_yaml = r#"extensions:
  developer:
    enabled: true
    type: platform
    name: developer
    description: Developer
    display_name: Developer
    bundled: true
    available_tools: []
"#
        .to_string();

        for ext in &config.extra_extensions {
            config_yaml.push_str(&format!(
                r#"  {ext}:
    enabled: true
    type: platform
    name: {ext}
    description: {ext}
    display_name: {ext}
    bundled: true
    available_tools: []
"#
            ));
        }
        std::fs::write(config_dir.join("config.yaml"), config_yaml).expect("write config.yaml");

        let mut rand_bytes = [0u8; 16];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut rand_bytes);
        }
        let secret: String = rand_bytes.iter().map(|b| format!("{:02x}", b)).collect();
        let secret_path = temp_path.join("secret");
        std::fs::write(&secret_path, &secret).expect("write secret");

        let dirs = ChildDirs {
            root_dir: &root_dir,
            home_dir: &home_dir,
            cwd_dir: &cwd_dir,
            temp_path: &temp_path,
        };

        let mut attempts = 0;
        let (child, pid, port) = loop {
            attempts += 1;
            let port = {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
                let p = listener.local_addr().expect("local addr").port();
                drop(listener);
                p
            };

            let (mut child, pid) =
                match spawn_child(&goose_bin, port, &secret, &dirs, &model.base_url()) {
                    Ok(res) => res,
                    Err(_e) if attempts < 3 => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(e) => panic!("failed to spawn goose: {e}"),
                };

            match wait_until_ready(port, &secret, Duration::from_secs(10)).await {
                Ok(()) => break (child, pid, port),
                Err(e) => {
                    kill_pgid(pid);
                    let _ = child.start_kill();
                    let mut status = 0;
                    unsafe {
                        libc::waitpid(pid as libc::pid_t, &mut status, 0);
                    }
                    if attempts >= 3 {
                        let log_path = temp_path.join("goose.log");
                        let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
                        eprintln!(
                            "goose readiness failed after 3 attempts: {e}\ngoose log:\n{log_content}"
                        );
                        panic!("goose readiness failed after 3 attempts: {e}");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };

        Some(Self {
            child: Some(child),
            pid,
            port,
            secret,
            secret_path,
            temp_dir,
            root_dir,
            home_dir,
            cwd_dir,
            goose_bin,
            model,
        })
    }

    pub fn log_text(&self) -> String {
        let p = self.temp_dir.path().join("goose.log");
        let p2 = self
            .temp_dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| self.temp_dir.path().to_path_buf())
            .join("goose.log");
        let s = std::fs::read_to_string(&p)
            .or_else(|_| std::fs::read_to_string(&p2))
            .unwrap_or_default();
        if s.is_empty() {
            format!(
                "[empty, exists p={}: {:?}, p2={}: {:?}]",
                p.exists(),
                std::fs::metadata(&p),
                p2.exists(),
                std::fs::metadata(&p2)
            )
        } else {
            s
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/acp", self.port)
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn secret_file(&self) -> &Path {
        &self.secret_path
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd_dir
    }

    pub fn root(&self) -> &Path {
        &self.root_dir
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn model(&self) -> &StubModel {
        &self.model
    }

    pub async fn export(&self, sid: &str) -> serde_json::Value {
        let mut up =
            Upstream::initialize(&self.url(), self.secret().to_string(), Recorder::disabled())
                .await
                .expect("initialize export upstream");
        up.subscribe(None).await.expect("subscribe export conn");
        up.subscribe(Some(sid))
            .await
            .expect("subscribe export session");

        let export_res = up
            .call(
                "_goose/unstable/session/export",
                serde_json::json!({
                    "sessionId": sid,
                    "format": "json"
                }),
                Some(sid),
                Duration::from_secs(30),
            )
            .await
            .expect("call export");

        let data_str = export_res
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("export missing data string: {export_res}"));

        let session_val: serde_json::Value =
            serde_json::from_str(data_str).expect("parse export data JSON");
        let _ = up.delete().await;
        session_val
    }

    pub async fn turn_text(&self, sid: &str) -> String {
        let export = self.export(sid).await;
        extract_turn_text(&export)
    }

    pub async fn tools_list(&self, sid: &str) -> Vec<String> {
        let mut up =
            Upstream::initialize(&self.url(), self.secret().to_string(), Recorder::disabled())
                .await
                .expect("initialize tools_list upstream");
        up.subscribe(None).await.expect("subscribe tools_list conn");
        up.subscribe(Some(sid))
            .await
            .expect("subscribe tools_list session");

        let res = up
            .call(
                "_goose/unstable/tools/list",
                serde_json::json!({
                    "sessionId": sid,
                }),
                Some(sid),
                Duration::from_secs(10),
            )
            .await;
        let _ = up.delete().await;

        res.ok()
            .and_then(|r| {
                r.pointer("/tools")
                    .and_then(serde_json::Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(|t| {
                                t.get("name")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .collect()
                    })
            })
            .unwrap_or_default()
    }

    pub fn kill9(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_pgid(self.pid);
            let _ = child.start_kill();
            let mut status = 0;
            unsafe {
                libc::waitpid(self.pid as libc::pid_t, &mut status, 0);
            }
        }
    }

    pub async fn restart(&mut self) {
        self.kill9();
        self.model.open_all();

        // Wait until port is released and available to bind again (retrying if in TIME_WAIT)
        let bind_start = Instant::now();
        let port_ready = loop {
            match std::net::TcpListener::bind(("127.0.0.1", self.port)) {
                Ok(listener) => {
                    drop(listener);
                    break true;
                }
                Err(_) => {
                    if bind_start.elapsed() > Duration::from_secs(10) {
                        break false;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        };
        if !port_ready {
            panic!(
                "timed out waiting for port {} to become reusable",
                self.port
            );
        }

        let temp_path = self
            .temp_dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| self.temp_dir.path().to_path_buf());
        let dirs = ChildDirs {
            root_dir: &self.root_dir,
            home_dir: &self.home_dir,
            cwd_dir: &self.cwd_dir,
            temp_path: &temp_path,
        };

        let (child, pid) = spawn_child(
            &self.goose_bin,
            self.port,
            &self.secret,
            &dirs,
            &self.model.base_url(),
        )
        .expect("respawn goose child on same port");

        self.child = Some(child);
        self.pid = pid;

        wait_until_ready(self.port, &self.secret, Duration::from_secs(10))
            .await
            .expect("goose ready after restart");
    }
}

impl Drop for StubGoose {
    fn drop(&mut self) {
        self.kill9();
    }
}

fn kill_pgid(pid: u32) {
    if let Ok(pgid) = libc::pid_t::try_from(pid) {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

struct ChildDirs<'a> {
    root_dir: &'a Path,
    home_dir: &'a Path,
    cwd_dir: &'a Path,
    temp_path: &'a Path,
}

fn spawn_child(
    goose_bin: &Path,
    port: u16,
    secret: &str,
    dirs: &ChildDirs<'_>,
    openai_host: &str,
) -> anyhow::Result<(tokio::process::Child, u32)> {
    let mut cmd = tokio::process::Command::new(goose_bin);
    cmd.arg("serve")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string());

    cmd.env_clear();
    cmd.env("HOME", dirs.home_dir);
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd.env("GOOSE_PATH_ROOT", dirs.root_dir);
    cmd.env("GOOSE_SERVER__SECRET_KEY", secret);
    cmd.env("GOOSE_PROVIDER", "openai");
    cmd.env("OPENAI_HOST", openai_host);
    cmd.env("OPENAI_API_KEY", "stub");
    cmd.env("GOOSE_MODEL", "stub-model");
    cmd.env("GOOSE_TELEMETRY_ENABLED", "false");
    cmd.env("GOOSE_DISABLE_SESSION_NAMING", "true");
    cmd.env("GOOSE_DISABLE_KEYRING", "1");
    cmd.env("RUST_LOG", "debug");
    cmd.current_dir(dirs.cwd_dir);

    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let log_file = std::fs::File::create(dirs.temp_path.join("goose.log"))?;
    cmd.stdout(log_file.try_clone()?);
    cmd.stderr(log_file);

    let child = cmd.spawn()?;
    let pid = child.id().expect("child pid");
    Ok((child, pid))
}

async fn wait_until_ready(port: u16, secret: &str, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/acp");
    let start = Instant::now();

    while start.elapsed() < timeout {
        let res = client
            .post(&url)
            .header("x-secret-key", secret)
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {}
                }
            }))
            .send()
            .await;

        if let Ok(r) = res
            && r.status().is_success()
        {
            if let Some(cid) = r
                .headers()
                .get("acp-connection-id")
                .and_then(|v| v.to_str().ok())
            {
                let _ = client
                    .delete(&url)
                    .header("x-secret-key", secret)
                    .header("acp-connection-id", cid)
                    .send()
                    .await;
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    anyhow::bail!("goose did not become ready within {timeout:?}")
}

pub fn extract_turn_text(export: &serde_json::Value) -> String {
    let conversation = match export.get("conversation").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return String::new(),
    };

    let is_user_prompt = |msg: &serde_json::Value| -> bool {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            return false;
        }
        if msg
            .pointer("/metadata/userVisible")
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            return false;
        }
        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
            let has_tool_response = content.iter().any(|item| {
                item.get("type").and_then(|t| t.as_str()) == Some("toolResponse")
                    || item.get("toolResult").is_some()
            });
            if has_tool_response {
                return false;
            }
            return content
                .iter()
                .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"));
        }
        false
    };

    let last_user_idx = conversation.iter().rposition(is_user_prompt);

    let msgs_after = match last_user_idx {
        Some(idx) => &conversation[idx + 1..],
        None => &conversation[..],
    };

    let mut out = String::new();
    for msg in msgs_after {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && let Some(contents) = msg.get("content").and_then(|c| c.as_array())
        {
            for item in contents {
                if item.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = item.get("text").and_then(|t| t.as_str())
                {
                    out.push_str(text);
                }
            }
        }
    }
    out
}
