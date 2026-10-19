use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "honkd", version)]
pub struct Args {
    #[arg(long, default_value = "127.0.0.1:4280")]
    pub listen: SocketAddr,
    #[arg(long, default_value = "http://127.0.0.1:4181/acp")]
    pub upstream: String,
    #[arg(long)]
    pub upstream_secret_file: PathBuf,
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long)]
    pub allow_production_upstream: bool,
    #[arg(long)]
    pub allow_unpatched_upstream: bool,
    #[arg(long)]
    pub discard_corrupt_state: bool,
    #[arg(long)]
    pub record: Option<PathBuf>,
    #[arg(long, default_value_t = 900, hide = true)]
    pub idle_ttl_secs: u64,
    #[arg(long, default_value_t = 4096, hide = true)]
    pub viewer_queue: usize,
    #[arg(long, default_value_t = 64, hide = true)]
    pub max_sessions: usize,
    #[arg(long, default_value_t = 20, hide = true)]
    pub stream_dead_secs: u64,
    #[arg(long, default_value_t = 2000, hide = true)]
    pub gap_synthesis_grace_ms: u64,
    #[arg(long, default_value_t = 5, hide = true)]
    pub probe_timeout_secs: u64,
    #[arg(long, default_value_t = 30, hide = true)]
    pub ambiguous_inflight_timeout_secs: u64,
    #[arg(long, default_value_t = 30, hide = true)]
    pub session_new_timeout_secs: u64,
    #[arg(long, hide = true)]
    pub session_new_timeout_ms: Option<u64>,
    #[arg(long)]
    pub turn_hook_url: Option<String>,
    #[arg(long)]
    pub turn_hook_token_file: Option<PathBuf>,
    #[arg(long, default_value_t = 1200, hide = true)]
    pub turn_hook_retry_secs: u64,
    #[arg(long)]
    pub mcp_catalog: Option<PathBuf>,
    #[arg(long = "mcp-no-confirm", value_delimiter = ',')]
    pub mcp_no_confirm: Vec<String>,
}

impl Args {
    pub fn resolve_state_dir(&self) -> PathBuf {
        if let Some(dir) = &self.state_dir {
            return dir.clone();
        }
        if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
            && !xdg.trim().is_empty()
        {
            return PathBuf::from(xdg.trim()).join("honk");
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.trim().is_empty()
        {
            return PathBuf::from(home.trim()).join(".local/state/honk");
        }
        PathBuf::from("./.honk-state")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_sessions_is_64() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.max_sessions, 64);
    }

    #[test]
    fn default_session_new_timeout_secs_is_30() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.session_new_timeout_secs, 30);
        assert_eq!(args.session_new_timeout_ms, None);
    }

    #[test]
    fn default_turn_hook_flags() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.turn_hook_url, None);
        assert_eq!(args.turn_hook_token_file, None);
        assert_eq!(args.turn_hook_retry_secs, 1200);
    }

    #[test]
    fn parsed_turn_hook_flags() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--turn-hook-url",
            "http://127.0.0.1:4096/goose/turn-event",
            "--turn-hook-token-file",
            "/tmp/token",
            "--turn-hook-retry-secs",
            "60",
        ])
        .expect("parse args");
        assert_eq!(
            args.turn_hook_url.as_deref(),
            Some("http://127.0.0.1:4096/goose/turn-event")
        );
        assert_eq!(args.turn_hook_token_file, Some(PathBuf::from("/tmp/token")));
        assert_eq!(args.turn_hook_retry_secs, 60);
    }

    #[test]
    fn default_mcp_flags() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.mcp_catalog, None);
        assert!(args.mcp_no_confirm.is_empty());
    }

    #[test]
    fn parsed_mcp_flags() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--mcp-catalog",
            "/tmp/opencode.json",
            "--mcp-no-confirm",
            "slack-ro,figma",
            "--mcp-no-confirm",
            "local-test",
        ])
        .expect("parse args");
        assert_eq!(args.mcp_catalog, Some(PathBuf::from("/tmp/opencode.json")));
        assert_eq!(args.mcp_no_confirm, vec!["slack-ro", "figma", "local-test"]);
    }
}
