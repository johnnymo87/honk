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
    /// Optional command invoked when an unheld session is activated to decide whether to switch its model
    #[arg(
        long,
        help = "Optional command invoked when an unheld session is activated to decide whether to switch its model"
    )]
    pub on_load_model_cmd: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 3,
        hide = true,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub on_load_model_timeout_secs: u64,
    #[arg(
        long,
        default_value_t = 2,
        hide = true,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub on_load_model_applied_timeout_secs: u64,
    #[arg(
        long,
        default_value_t = 10,
        hide = true,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub on_load_model_rpc_timeout_secs: u64,
    #[arg(long)]
    pub mcp_catalog: Option<PathBuf>,
    /// Path to JSON file defining configured models per provider
    #[arg(
        long,
        help = "Path to JSON file defining configured models per provider"
    )]
    pub models_file: Option<PathBuf>,
    #[arg(long = "mcp-no-confirm", value_delimiter = ',')]
    pub mcp_no_confirm: Vec<String>,
    /// `SERVER=TOOL,TOOL,...`: when honkd enables SERVER in a goose session, goose
    /// exposes only these tools from it (goose's per-extension `available_tools`
    /// allowlist; any other tool is absent from the tool list and cannot be called).
    /// Repeatable, one server per flag. A server with no entry gets every tool.
    #[arg(long = "mcp-allow-tools", value_name = "SERVER=TOOL,...")]
    pub mcp_allow_tools: Vec<String>,
    /// An executable with mcp-remote's command line. With it, catalog remote
    /// servers that send headers ({file:} secrets) or may need OAuth run through
    /// it as stdio servers.
    #[arg(long, value_name = "PATH")]
    pub mcp_remote_bin: Option<PathBuf>,
    #[arg(long, help = "Allow agents to self-grant MCP servers via POST /mcp/*")]
    pub mcp_self_grant: bool,
    #[arg(
        long,
        help = "Enable swarm MCP by default on session/new (requires --swarm-mcp-bin and --swarm-pigeon-url)"
    )]
    pub swarm_default: bool,
    #[arg(
        long,
        help = "Enable the session cwd's opencode.json MCP servers on session/new (loopback remote servers only)"
    )]
    pub project_mcp: bool,
    #[arg(
        long,
        help = "session-tags binary: tag each _honk/oneshot session like the session it serves (tagFrom), so its spend lands under that program"
    )]
    pub session_tags_bin: Option<PathBuf>,
    #[arg(long, help = "Path to honk binary for built-in swarm MCP server")]
    pub swarm_mcp_bin: Option<PathBuf>,
    #[arg(long, help = "Pigeon daemon base URL for swarm MCP")]
    pub swarm_pigeon_url: Option<String>,
    #[arg(long, help = "Path to pigeon auth token file for swarm MCP")]
    pub swarm_pigeon_token_file: Option<PathBuf>,
}

impl Args {
    /// Parses `--mcp-allow-tools` into server name -> sorted, deduplicated tools.
    ///
    /// Fails on a malformed value, a duplicate server, or an empty tool list: goose
    /// reads an empty `available_tools` as "every tool", so an empty list must
    /// never be passed through as if it restricted anything.
    pub fn mcp_allow_tools_map(
        &self,
    ) -> anyhow::Result<std::collections::BTreeMap<String, Vec<String>>> {
        let mut map = std::collections::BTreeMap::new();
        for raw in &self.mcp_allow_tools {
            let Some((server, tools)) = raw.split_once('=') else {
                anyhow::bail!("--mcp-allow-tools '{raw}': expected SERVER=TOOL,TOOL,...");
            };
            let server = server.trim();
            if server.is_empty() {
                anyhow::bail!("--mcp-allow-tools '{raw}': server name is empty");
            }
            let mut list = Vec::new();
            for tool in tools.split(',') {
                let tool = tool.trim();
                if tool.is_empty() {
                    anyhow::bail!("--mcp-allow-tools '{raw}': empty tool name");
                }
                list.push(tool.to_string());
            }
            list.sort();
            list.dedup();
            if map.insert(server.to_string(), list).is_some() {
                anyhow::bail!("--mcp-allow-tools: server '{server}' is given more than once");
            }
        }
        Ok(map)
    }

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
    fn default_on_load_model_flags() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.on_load_model_cmd, None);
        assert_eq!(args.on_load_model_timeout_secs, 3);
        assert_eq!(args.on_load_model_applied_timeout_secs, 2);
        assert_eq!(args.on_load_model_rpc_timeout_secs, 10);
    }

    #[test]
    fn parsed_on_load_model_flags() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--on-load-model-cmd",
            "/usr/local/bin/model-policy",
            "--on-load-model-timeout-secs",
            "5",
            "--on-load-model-applied-timeout-secs",
            "4",
            "--on-load-model-rpc-timeout-secs",
            "15",
        ])
        .expect("parse args");
        assert_eq!(
            args.on_load_model_cmd,
            Some(PathBuf::from("/usr/local/bin/model-policy"))
        );
        assert_eq!(args.on_load_model_timeout_secs, 5);
        assert_eq!(args.on_load_model_applied_timeout_secs, 4);
        assert_eq!(args.on_load_model_rpc_timeout_secs, 15);
    }

    #[test]
    fn rejects_zero_on_load_model_timeouts() {
        assert!(
            Args::try_parse_from([
                "honkd",
                "--upstream-secret-file",
                "/tmp/secret",
                "--on-load-model-timeout-secs",
                "0",
            ])
            .is_err()
        );

        assert!(
            Args::try_parse_from([
                "honkd",
                "--upstream-secret-file",
                "/tmp/secret",
                "--on-load-model-applied-timeout-secs",
                "0",
            ])
            .is_err()
        );

        assert!(
            Args::try_parse_from([
                "honkd",
                "--upstream-secret-file",
                "/tmp/secret",
                "--on-load-model-rpc-timeout-secs",
                "0",
            ])
            .is_err()
        );
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
        assert_eq!(args.models_file, None);
        assert!(args.mcp_no_confirm.is_empty());
        assert!(!args.mcp_self_grant);
        assert!(!args.project_mcp);
        assert_eq!(args.mcp_remote_bin, None);
    }

    #[test]
    fn parsed_models_file_flag() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--models-file",
            "/tmp/models.json",
        ])
        .expect("parse args");
        assert_eq!(args.models_file, Some(PathBuf::from("/tmp/models.json")));
    }

    #[test]
    fn parsed_project_mcp_flag() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--project-mcp",
        ])
        .expect("parse args");
        assert!(args.project_mcp);
        assert_eq!(args.mcp_catalog, None);
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
            "--mcp-self-grant",
        ])
        .expect("parse args");
        assert_eq!(args.mcp_catalog, Some(PathBuf::from("/tmp/opencode.json")));
        assert_eq!(args.mcp_no_confirm, vec!["slack-ro", "figma", "local-test"]);
        assert!(args.mcp_self_grant);
    }

    #[test]
    fn default_swarm_flags() {
        let args = Args::try_parse_from(["honkd", "--upstream-secret-file", "/tmp/secret"])
            .expect("parse args");
        assert_eq!(args.swarm_mcp_bin, None);
        assert_eq!(args.swarm_pigeon_url, None);
        assert_eq!(args.swarm_pigeon_token_file, None);
        assert!(!args.swarm_default);
    }

    #[test]
    fn parsed_swarm_flags() {
        let args = Args::try_parse_from([
            "honkd",
            "--upstream-secret-file",
            "/tmp/secret",
            "--swarm-default",
            "--swarm-mcp-bin",
            "/etc/profiles/per-user/dev/bin/honk",
            "--swarm-pigeon-url",
            "http://127.0.0.1:4731",
            "--swarm-pigeon-token-file",
            "/tmp/pigeon-token",
        ])
        .expect("parse args");
        assert!(args.swarm_default);
        assert_eq!(
            args.swarm_mcp_bin,
            Some(PathBuf::from("/etc/profiles/per-user/dev/bin/honk"))
        );
        assert_eq!(
            args.swarm_pigeon_url.as_deref(),
            Some("http://127.0.0.1:4731")
        );
        assert_eq!(
            args.swarm_pigeon_token_file,
            Some(PathBuf::from("/tmp/pigeon-token"))
        );
    }
    fn allow_tools_args(values: &[&str]) -> Args {
        let mut argv = vec!["honkd", "--upstream-secret-file", "/tmp/secret"];
        for v in values {
            argv.push("--mcp-allow-tools");
            argv.push(v);
        }
        Args::try_parse_from(argv).expect("parse args")
    }

    #[test]
    fn default_mcp_allow_tools_is_empty() {
        let args = allow_tools_args(&[]);
        assert!(args.mcp_allow_tools_map().unwrap().is_empty());
    }

    #[test]
    fn parsed_mcp_allow_tools_sorted_and_deduped() {
        let args = allow_tools_args(&[
            "slack-ro=users_search,channels_list,users_search",
            "figma=get_file",
        ]);
        let map = args.mcp_allow_tools_map().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["slack-ro"], vec!["channels_list", "users_search"]);
        assert_eq!(map["figma"], vec!["get_file"]);
    }

    #[test]
    fn parsed_mcp_allow_tools_trims_blanks() {
        let args = allow_tools_args(&[" slack-ro = channels_list , users_search "]);
        let map = args.mcp_allow_tools_map().unwrap();
        assert_eq!(map["slack-ro"], vec!["channels_list", "users_search"]);
    }

    #[test]
    fn mcp_allow_tools_rejects_malformed_values() {
        // An empty list would reach goose as "no allowlist" (every tool), so it
        // must be an error rather than a silently permissive entry.
        for bad in [
            "slack-ro",
            "slack-ro=",
            "slack-ro= , ",
            "=channels_list",
            "slack-ro=a,,b",
        ] {
            let args = allow_tools_args(&[bad]);
            assert!(
                args.mcp_allow_tools_map().is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn mcp_allow_tools_rejects_duplicate_server() {
        let args = allow_tools_args(&["slack-ro=a", "slack-ro=b"]);
        let err = args.mcp_allow_tools_map().unwrap_err().to_string();
        assert!(err.contains("slack-ro"), "{err}");
    }
}
