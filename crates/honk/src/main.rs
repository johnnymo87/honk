use clap::{Args, Parser, Subcommand};
use honk::{Client, TailOpts, resolve_secret};
use std::io::stdout;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "honk", about = "CLI client for honk relay", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Tail a session transcript in real time
    Tail(TailArgs),
    /// List sessions tracked by honkd
    List(ListArgs),
    /// Interactive ratatui TUI client for a goose session
    #[command(
        long_about = "Interactive ratatui TUI client for a goose session.\n\nSee docs/chat.md for key map, commands, shared session rules, and tmux configuration."
    )]
    Chat(ChatArgs),
    /// Register an existing session with pigeon so it gets a Telegram topic
    #[command(
        long_about = "Register an existing session with a pigeon daemon so it gets a Telegram topic.\n\nhonk chat --new does this automatically when HONK_PIGEON_URL is set. Telegram replies prompt the session; turns typed in honk chat are not mirrored to Telegram. pigeon is given the honkd secret so it can reach the session."
    )]
    Telegram(TelegramArgs),
    /// Run the swarm MCP server over stdio for goose
    #[command(name = "swarm-mcp")]
    SwarmMcp(SwarmMcpArgs),
}

/// Where the pigeon daemon is. Registration is off unless a URL is given.
#[derive(Args, Debug, Clone)]
struct PigeonArgs {
    /// pigeon daemon URL; when set, `chat --new` registers the session with it
    #[arg(long = "pigeon-url", env = "HONK_PIGEON_URL")]
    pigeon_url: Option<String>,

    /// File holding pigeon's bearer token, if the daemon requires one
    #[arg(long = "pigeon-token-file", env = "HONK_PIGEON_TOKEN_FILE")]
    pigeon_token_file: Option<PathBuf>,
}

impl PigeonArgs {
    fn settings(&self) -> Option<honk::pigeon::PigeonSettings> {
        let url = self.pigeon_url.as_deref()?.trim();
        if url.is_empty() {
            return None;
        }
        Some(honk::pigeon::PigeonSettings {
            url: url.to_string(),
            token_file: self.pigeon_token_file.clone(),
        })
    }
}

#[derive(Args, Debug)]
struct TelegramArgs {
    /// Session ID to register
    session: String,

    /// Working directory to report, if honkd's session list does not include the session
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Register even if pigeon already has the session (overwrites pigeon's row,
    /// including the token of a session pigeon launched itself)
    #[arg(long)]
    force: bool,

    #[command(flatten)]
    pigeon: PigeonArgs,

    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,

    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
}

#[derive(Args, Debug)]
struct SwarmMcpArgs {
    /// Owning goose session ID
    #[arg(long)]
    session: String,

    /// pigeon daemon URL
    #[arg(long, default_value = "http://127.0.0.1:4731", env = "HONK_PIGEON_URL")]
    pigeon_url: String,

    /// File holding pigeon's bearer token, if the daemon requires one
    #[arg(long, env = "HONK_PIGEON_TOKEN_FILE")]
    pigeon_token_file: Option<PathBuf>,

    /// honkd ACP URL
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    honk_url: String,

    /// File holding honkd's auth token
    #[arg(long, env = "HONK_SECRET_FILE")]
    honk_secret_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ChatArgs {
    /// Session ID to attach to
    #[arg(conflicts_with = "new", required_unless_present = "new")]
    session: Option<String>,

    /// Start a new session
    #[arg(long, conflicts_with = "session")]
    new: bool,

    /// Working directory for a new session
    #[arg(long, requires = "new")]
    cwd: Option<PathBuf>,

    /// Extensions to enable for a new session (repeatable)
    #[arg(long = "ext", requires = "new", conflicts_with = "session")]
    ext: Vec<String>,

    /// MCP servers to enable for a new session (repeatable, append '!' to confirm)
    #[arg(long = "mcp", requires = "new", conflicts_with = "session")]
    mcp: Vec<String>,

    /// Cap model calls per turn (positive integer)
    #[arg(long = "max-turns", env = "HONK_MAX_TURNS", value_parser = parse_max_turns)]
    max_turns: Option<u32>,

    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,

    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,

    /// Do not register a new session with pigeon, even if HONK_PIGEON_URL is set
    #[arg(long = "no-telegram")]
    no_telegram: bool,

    /// Tag the new session in oc-tags as goose:<id> (like opencode-launch --tag),
    /// so it is listed under that program. Runs $HONK_OC_TAGS_BIN or oc-tags.
    #[arg(long, requires = "new", conflicts_with = "session", value_parser = honk::octags::parse_tag)]
    tag: Option<String>,

    #[command(flatten)]
    pigeon: PigeonArgs,
}

#[derive(Args, Debug)]
struct TailArgs {
    /// Session ID to tail
    session: String,
    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,
    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
    /// Agent text only (suppresses tool lines, markers, user lines).
    /// Note: on first attach to an existing session, prints the entire session's
    /// agent text (not just the last turn).
    #[arg(long)]
    text_only: bool,
    /// Include thought blocks in output
    #[arg(long)]
    thoughts: bool,
    /// Exit once an active turn finishes.
    /// If the session is idle at attach, waits for the next turn to start and finish before exiting.
    #[arg(long)]
    exit_on_turn_end: bool,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Filter sessions by working directory
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Print one JSON array instead of text lines. Each object has sessionId,
    /// title, cwd, held, running, updatedAt, lastMessageAt, archivedAt,
    /// sessionType, hasRecipe, pendingRequests and lastError (unknown values are null).
    #[arg(long)]
    json: bool,

    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,
    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
}

fn parse_max_turns(s: &str) -> Result<u32, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("invalid --max-turns `{s}`: must be an integer >= 1"))?;
    if n < 1 {
        return Err(format!("invalid --max-turns `{s}`: must be >= 1"));
    }
    Ok(n)
}

fn resolve_list_cwd(p: &std::path::Path) -> String {
    if let Ok(canon) = p.canonicalize() {
        canon.to_string_lossy().to_string()
    } else {
        p.to_string_lossy().to_string()
    }
}

fn resolve_chat_target(args: &ChatArgs) -> anyhow::Result<honk::chat::driver::Target> {
    if args.new {
        let cwd = match &args.cwd {
            Some(c) => c
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("invalid --cwd: {}", e))?,
            None => std::env::current_dir()?.canonicalize()?,
        };
        let extensions = if args.ext.is_empty() {
            None
        } else {
            Some(args.ext.clone())
        };
        let mcp = if args.mcp.is_empty() {
            None
        } else {
            let mut confirm = Vec::new();
            let mut names = Vec::new();
            for item in &args.mcp {
                if let Some(stripped) = item.strip_suffix('!') {
                    confirm.push(stripped.to_string());
                    names.push(stripped.to_string());
                } else {
                    names.push(item.clone());
                }
            }
            Some(honk::chat::driver::McpLaunchConfig { names, confirm })
        };
        Ok(honk::chat::driver::Target::New {
            cwd,
            extensions,
            mcp,
        })
    } else {
        if !args.ext.is_empty() {
            anyhow::bail!("--ext requires --new");
        }
        if !args.mcp.is_empty() {
            anyhow::bail!("--mcp requires --new");
        }
        Ok(honk::chat::driver::Target::Attach(
            args.session.clone().unwrap(),
        ))
    }
}

/// Only a session this invocation creates is registered: attaching to an
/// existing one would re-announce it on every attach (`honk telegram` exists
/// for that).
fn chat_pigeon_settings(args: &ChatArgs) -> Option<honk::pigeon::PigeonSettings> {
    if args.new && !args.no_telegram {
        args.pigeon.settings()
    } else {
        None
    }
}

/// Only a session this invocation creates is tagged; `--tag` requires `--new`.
fn chat_tag_settings(args: &ChatArgs) -> Option<honk::octags::TagSettings> {
    args.tag.clone().map(honk::octags::TagSettings::new)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Tail(args) => {
            let secret = resolve_secret(args.secret.as_deref())?;
            let opts = TailOpts {
                url: args.url,
                secret,
                session: args.session,
                text_only: args.text_only,
                thoughts: args.thoughts,
                exit_on_turn_end: args.exit_on_turn_end,
            };
            if let Err(e) = honk::run(opts, stdout()).await {
                if let Some(io_err) = e.downcast_ref::<std::io::Error>()
                    && io_err.kind() == std::io::ErrorKind::BrokenPipe
                {
                    std::process::exit(0);
                }
                return Err(e);
            }
        }
        Commands::List(args) => {
            let secret = resolve_secret(args.secret.as_deref())?;
            let mut client = Client::connect(&args.url, &secret).await?;
            let cwd = args.cwd.as_deref().map(resolve_list_cwd);
            let opts = honk::ListOpts {
                cwd,
                ..Default::default()
            };
            let sessions = client.list_sessions_with(opts).await?;
            let lines = if args.json {
                vec![honk::client::sessions_json(&sessions)]
            } else {
                sessions.iter().map(|s| s.format_line()).collect()
            };
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            use std::io::Write;
            for line in lines {
                if let Err(e) = writeln!(handle, "{line}") {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        std::process::exit(0);
                    }
                    return Err(e.into());
                }
            }
        }
        Commands::Chat(args) => {
            let secret = resolve_secret(args.secret.as_deref())?;
            let target = resolve_chat_target(&args)?;

            let session_id = match &target {
                honk::chat::driver::Target::Attach(id) => id.clone(),
                honk::chat::driver::Target::New { .. } => String::new(),
            };

            let pigeon = chat_pigeon_settings(&args);
            let tag = chat_tag_settings(&args);
            let mut opts = honk::chat::driver::DriverOpts::new(args.url, secret, target);
            opts.max_turns = args.max_turns;
            opts.pigeon = pigeon;
            opts.tag = tag;
            let (handle, ui_rx) = honk::chat::driver::start(opts).await?;
            let app = honk::tui::App::new(session_id, handle.cmd_tx());
            honk::tui::app::run_app(app, ui_rx, handle).await?;
        }
        Commands::Telegram(args) => {
            let Some(settings) = args.pigeon.settings() else {
                anyhow::bail!(
                    "no pigeon daemon configured: set HONK_PIGEON_URL or pass --pigeon-url"
                );
            };
            let cfg = settings.load()?;
            if !args.force
                && let Some(existing) = honk::pigeon::find_registered(&cfg, &args.session).await?
            {
                anyhow::bail!(
                    "pigeon already has session {} (as {existing}); pass --force to re-register it",
                    args.session
                );
            }
            let secret = resolve_secret(args.secret.as_deref())?;
            let endpoint = honk::pigeon::acp_endpoint(&args.url)?;
            let mut client = Client::connect(&args.url, &secret).await?;
            let listed = client
                .list_sessions()
                .await?
                .into_iter()
                .find(|s| s.session_id == args.session);
            let title = listed
                .as_ref()
                .map(|s| s.title.clone())
                .filter(|t| !t.is_empty());
            let cwd = match (&args.cwd, listed) {
                (Some(c), _) => resolve_list_cwd(c),
                (None, Some(s)) => s.cwd,
                (None, None) => anyhow::bail!(
                    "session {} is not in honkd's session list; pass --cwd to register it anyway",
                    args.session
                ),
            };
            let reg = honk::pigeon::Registration {
                goose_session_id: args.session,
                endpoint,
                auth_token: secret,
                cwd,
                title,
                reason: honk::pigeon::Reason::Existing,
            };
            let out = honk::pigeon::register(&cfg, &reg).await?;
            println!("{} (pigeon id {})", out.summary(), out.pigeon_session_id);
        }
        Commands::SwarmMcp(args) => {
            let config = honk::swarm_mcp::SwarmMcpServerConfig {
                session: args.session,
                pigeon_url: args.pigeon_url,
                pigeon_token_file: args.pigeon_token_file,
                honk_url: args.honk_url,
                honk_secret_file: args.honk_secret_file,
            };
            honk::swarm_mcp::run_stdio(config).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_target_nonexistent_cwd_errors() {
        let args = ChatArgs {
            session: None,
            new: true,
            cwd: Some(PathBuf::from("/nonexistent_path_xyz_123")),
            ext: vec![],
            mcp: vec![],
            max_turns: None,
            url: "http://127.0.0.1:4280/acp".into(),
            secret: None,
            no_telegram: false,
            tag: None,
            pigeon: PigeonArgs {
                pigeon_url: None,
                pigeon_token_file: None,
            },
        };
        let err = resolve_chat_target(&args).unwrap_err();
        assert!(err.to_string().contains("invalid --cwd:"));
    }

    #[test]
    fn test_chat_target_existing_cwd_canonicalized() {
        let temp = tempfile::tempdir().unwrap();
        let args = ChatArgs {
            session: None,
            new: true,
            cwd: Some(temp.path().to_path_buf()),
            ext: vec!["dev".into()],
            mcp: vec![],
            max_turns: None,
            url: "http://127.0.0.1:4280/acp".into(),
            secret: None,
            no_telegram: false,
            tag: None,
            pigeon: PigeonArgs {
                pigeon_url: None,
                pigeon_token_file: None,
            },
        };
        let target = resolve_chat_target(&args).unwrap();
        match target {
            honk::chat::driver::Target::New {
                cwd, extensions, ..
            } => {
                assert_eq!(cwd, temp.path().canonicalize().unwrap());
                assert_eq!(extensions, Some(vec!["dev".into()]));
            }
            _ => panic!("expected Target::New"),
        }
    }

    #[test]
    fn test_cli_parse_tag_flag() {
        let cli = Cli::try_parse_from(["honk", "chat", "--new", "--tag", "honk"]).unwrap();
        match cli.command {
            Commands::Chat(args) => {
                let tag = chat_tag_settings(&args).expect("tag settings");
                assert_eq!(tag.tag, "honk");
            }
            _ => panic!("expected chat command"),
        }
        let cli = Cli::try_parse_from(["honk", "chat", "--new"]).unwrap();
        match cli.command {
            Commands::Chat(args) => assert!(chat_tag_settings(&args).is_none()),
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_tag_requires_new_and_a_valid_tag() {
        // Attaching does not tag: the session already has whatever tag it has.
        assert!(Cli::try_parse_from(["honk", "chat", "20261020_3", "--tag", "honk"]).is_err());
        assert!(Cli::try_parse_from(["honk", "chat", "--tag", "honk"]).is_err());
        // A bad tag is refused before any session exists.
        for bad in ["auto:x", "auto", "has space", "-x", ""] {
            assert!(
                Cli::try_parse_from(["honk", "chat", "--new", "--tag", bad]).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn test_cli_parse_mcp_flags() {
        let temp = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "honk",
            "chat",
            "--new",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--mcp",
            "slack-ro",
            "--mcp",
            "slack!",
        ])
        .unwrap();
        match cli.command {
            Commands::Chat(args) => {
                let target = resolve_chat_target(&args).unwrap();
                match target {
                    honk::chat::driver::Target::New { mcp, .. } => {
                        assert_eq!(
                            mcp,
                            Some(honk::chat::driver::McpLaunchConfig {
                                names: vec!["slack-ro".into(), "slack".into()],
                                confirm: vec!["slack".into()],
                            })
                        );
                    }
                    _ => panic!("expected Target::New"),
                }
            }
            _ => panic!("expected chat command"),
        }

        let cli2 =
            Cli::try_parse_from(["honk", "chat", "--new", "--mcp", "a!", "--mcp", "b"]).unwrap();
        match cli2.command {
            Commands::Chat(args) => {
                let target = resolve_chat_target(&args).unwrap();
                match target {
                    honk::chat::driver::Target::New { mcp, .. } => {
                        assert_eq!(
                            mcp,
                            Some(honk::chat::driver::McpLaunchConfig {
                                names: vec!["a".into(), "b".into()],
                                confirm: vec!["a".into()],
                            })
                        );
                    }
                    _ => panic!("expected Target::New"),
                }
            }
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_requires_new() {
        let res = Cli::try_parse_from(["honk", "chat", "ses_1", "--mcp", "slack-ro"]);
        assert!(res.is_err());
    }

    #[test]
    fn test_cli_parse_list_json() {
        let cli = Cli::try_parse_from(["honk", "list", "--json"]).unwrap();
        match cli.command {
            Commands::List(args) => assert!(args.json),
            _ => panic!("expected list command"),
        }
        let cli = Cli::try_parse_from(["honk", "list"]).unwrap();
        match cli.command {
            Commands::List(args) => assert!(!args.json),
            _ => panic!("expected list command"),
        }
    }

    #[test]
    fn test_cli_parse_max_turns_valid() {
        let cli = Cli::try_parse_from(["honk", "chat", "ses_1", "--max-turns", "5"]).unwrap();
        match cli.command {
            Commands::Chat(args) => assert_eq!(args.max_turns, Some(5)),
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_parse_max_turns_default_unset() {
        let cli = Cli::try_parse_from(["honk", "chat", "ses_1"]).unwrap();
        match cli.command {
            Commands::Chat(args) => assert_eq!(args.max_turns, None),
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_parse_max_turns_zero_fails() {
        let err = Cli::try_parse_from(["honk", "chat", "ses_1", "--max-turns", "0"]).unwrap_err();
        assert!(err.to_string().contains(">= 1"));
    }

    fn chat(argv: &[&str]) -> ChatArgs {
        let mut full = vec!["honk", "chat"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command {
            Commands::Chat(a) => a,
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_chat_new_registers_with_pigeon_when_url_given() {
        let a = chat(&[
            "--new",
            "--pigeon-url",
            "http://127.0.0.1:4731",
            "--pigeon-token-file",
            "/t",
        ]);
        assert_eq!(
            chat_pigeon_settings(&a),
            Some(honk::pigeon::PigeonSettings {
                url: "http://127.0.0.1:4731".into(),
                token_file: Some(PathBuf::from("/t")),
            })
        );
    }

    #[test]
    fn test_chat_pigeon_off_without_url_with_opt_out_or_on_attach() {
        // No URL given (and none in the environment of a test run that sets it
        // would be a misconfigured test run, so pass an empty one explicitly).
        assert_eq!(
            chat_pigeon_settings(&chat(&["--new", "--pigeon-url", ""])),
            None
        );
        assert_eq!(
            chat_pigeon_settings(&chat(&[
                "--new",
                "--no-telegram",
                "--pigeon-url",
                "http://p"
            ])),
            None
        );
        assert_eq!(
            chat_pigeon_settings(&chat(&["ses_1", "--pigeon-url", "http://p"])),
            None
        );
    }

    #[test]
    fn test_cli_parse_telegram_subcommand() {
        let cli = Cli::try_parse_from([
            "honk",
            "telegram",
            "20261016_1",
            "--pigeon-url",
            "http://p",
            "--cwd",
            "/w",
        ])
        .unwrap();
        match cli.command {
            Commands::Telegram(a) => {
                assert_eq!(a.session, "20261016_1");
                assert_eq!(a.cwd, Some(PathBuf::from("/w")));
                assert_eq!(a.pigeon.settings().unwrap().url, "http://p");
            }
            _ => panic!("expected telegram command"),
        }
    }

    #[test]
    fn test_cli_parse_swarm_mcp_subcommand() {
        let cli = Cli::try_parse_from([
            "honk",
            "swarm-mcp",
            "--session",
            "20261020_1",
            "--pigeon-url",
            "http://127.0.0.1:4731",
            "--pigeon-token-file",
            "/tmp/tok",
            "--honk-url",
            "http://127.0.0.1:4280/acp",
            "--honk-secret-file",
            "/tmp/secret",
        ])
        .unwrap();
        match cli.command {
            Commands::SwarmMcp(a) => {
                assert_eq!(a.session, "20261020_1");
                assert_eq!(a.pigeon_url, "http://127.0.0.1:4731");
                assert_eq!(a.pigeon_token_file, Some(PathBuf::from("/tmp/tok")));
                assert_eq!(a.honk_url, "http://127.0.0.1:4280/acp");
                assert_eq!(a.honk_secret_file, Some(PathBuf::from("/tmp/secret")));
            }
            _ => panic!("expected swarm-mcp command"),
        }
    }

    #[test]
    fn test_resolve_list_cwd_existing_canonicalizes() {
        let temp = tempfile::tempdir().unwrap();
        let canon = temp.path().canonicalize().unwrap();
        assert_eq!(resolve_list_cwd(temp.path()), canon.to_string_lossy());
    }

    #[test]
    fn test_resolve_list_cwd_nonexistent_verbatim() {
        let non_existent = PathBuf::from("/nonexistent/path/xyz/123");
        assert_eq!(resolve_list_cwd(&non_existent), "/nonexistent/path/xyz/123");
    }
}
