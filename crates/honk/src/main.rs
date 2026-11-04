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
    /// Start a goose session headlessly: tag, register with pigeon, prompt, detach
    #[command(long_about = LAUNCH_LONG_ABOUT)]
    Launch(LaunchArgs),
}

const LAUNCH_LONG_ABOUT: &str = "\
Start a goose session headlessly, the goose analogue of opencode-launch.

Creates the session through honkd with swarm tools on, tags it in session-tags \
as goose:<id>, registers it with pigeon (a Telegram topic, notify on, honkd as \
its push endpoint, so swarm_send reaches it), sends the prompt, and exits while \
the turn runs.

Tag: --tag <tag> wins. Without --tag the launching session's tag is inherited \
($OPENCODE_SESSION_ID, or goose:$AGENT_SESSION_ID); a launcher that is a \
stall-watch coordinator gives its program's tag, never its own \
'stall-watcher-runs'. --tag auto applies no tag.

Output: on stdout, exactly one line once a session exists:
  native=<goose id> gse=<pigeon id or -> tag=<tag or -> prompt=<ok|no|unknown>
Everything else goes to stderr. Put `--` before <dir> if the prompt may start \
with `-`, or pass `-` and the prompt on stdin.

Exit codes:
  0  launched: tagged as asked, registered, prompted, turn started
  1  no session was created (bad input, pigeon unreachable or refusing the \
token, tag undecidable, session-tags missing, honkd unreachable; a session/new \
timeout may still have created one, see `honk list`). No stdout line.
  2  usage error
  3  the session exists but a step failed (swarm, title, tag, pigeon, prompt \
refused); the line says how far it got. A tag failure does not stop the launch: \
it may be prompt=ok with tag=-.
  4  as 3, but the prompt was sent and not seen to start (prompt=unknown): the \
turn may be running. Do not relaunch the same task.";

#[derive(Args, Debug)]
struct LaunchArgs {
    /// Working directory for the session
    dir: PathBuf,

    /// The first prompt; `-` reads it from stdin
    prompt: String,

    /// Tag the session in session-tags (goose:<id>); `auto` for no tag.
    /// Default: inherit the launching session's tag.
    #[arg(long, value_parser = honk::octags::parse_tag_arg)]
    tag: Option<honk::octags::TagArg>,

    /// Session title, set before the first prompt (also names the Telegram topic)
    #[arg(long)]
    title: Option<String>,

    /// pigeon daemon URL
    #[arg(
        long = "pigeon-url",
        default_value = "http://127.0.0.1:4731",
        env = "HONK_PIGEON_URL"
    )]
    pigeon_url: String,

    /// File holding pigeon's bearer token [default: /run/secrets/pigeon_daemon_auth_token, if it exists]
    #[arg(long = "pigeon-token-file", env = "HONK_PIGEON_TOKEN_FILE")]
    pigeon_token_file: Option<PathBuf>,

    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,

    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
}

/// Where cloudbox keeps pigeon's daemon token (honkd reads the same file). The
/// session variable naming it reaches login shells only, not goose's or
/// opencode's tool shells, which is where `honk launch` runs.
const DEFAULT_PIGEON_TOKEN_FILE: &str = "/run/secrets/pigeon_daemon_auth_token";

fn launch_opts(
    args: LaunchArgs,
    env_get: impl Fn(&str) -> Option<String>,
    read_stdin: impl FnOnce() -> std::io::Result<String>,
) -> anyhow::Result<honk::launch::LaunchOpts> {
    let dir = args
        .dir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("invalid dir {}: {e}", args.dir.display()))?;
    if !dir.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    let prompt = if args.prompt == "-" {
        read_stdin().map_err(|e| anyhow::anyhow!("reading the prompt from stdin: {e}"))?
    } else {
        args.prompt
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("the prompt is empty");
    }
    let title = match args.title {
        Some(t) if t.trim().is_empty() => anyhow::bail!("--title is empty"),
        Some(t) => Some(t.trim().to_string()),
        None => None,
    };
    let tag = match args.tag {
        None => honk::launch::TagRequest::Inherit,
        Some(honk::octags::TagArg::NoInherit) => honk::launch::TagRequest::Off,
        Some(honk::octags::TagArg::Explicit(t)) => honk::launch::TagRequest::Explicit(t),
    };
    let token_file = args.pigeon_token_file.or_else(|| {
        let p = PathBuf::from(DEFAULT_PIGEON_TOKEN_FILE);
        p.is_file().then_some(p)
    });
    let pigeon = honk::pigeon::PigeonSettings {
        url: args.pigeon_url,
        token_file,
    }
    .load()?;
    let secret = resolve_secret(args.secret.as_deref())?;
    Ok(honk::launch::LaunchOpts {
        dir,
        prompt,
        title,
        tag,
        launcher: honk::octags::resolve_launcher_from(&env_get),
        tags_bin: honk::octags::resolve_bin_from(&env_get, honk::octags::on_path),
        honk_url: args.url,
        secret,
        pigeon,
        timeouts: honk::launch::Timeouts::default(),
    })
}

async fn run_launch(args: LaunchArgs) -> i32 {
    let opts = match launch_opts(
        args,
        |k| std::env::var(k).ok(),
        || std::io::read_to_string(std::io::stdin()),
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("honk launch: {e:#}");
            return honk::launch::EXIT_NOT_CREATED;
        }
    };
    match honk::launch::run(&opts).await {
        Ok(launched) => {
            println!("{}", launched.line());
            honk::launch::EXIT_OK
        }
        Err(e) => {
            let code = e.exit_code();
            match e {
                honk::launch::LaunchError::NotCreated(err) => {
                    eprintln!("honk launch: {err:#}");
                }
                honk::launch::LaunchError::Partial { launched, error } => {
                    println!("{}", launched.line());
                    eprintln!(
                        "honk launch: session {} created, but {error:#}",
                        launched.native
                    );
                }
            }
            code
        }
    }
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

    /// Do not enable the swarm MCP server by default on a new session (sends _meta.honk.swarm: false)
    #[arg(long = "no-swarm")]
    no_swarm: bool,

    /// Tag the new session in oc-tags as goose:<id> (like opencode-launch --tag).
    /// Inherits the launching session's tag by default if set; pass --tag <tag>
    /// to set explicitly, or --tag auto to opt out of inheritance. Runs
    /// $HONK_OC_TAGS_BIN or oc-tags.
    #[arg(long, requires = "new", conflicts_with = "session", value_parser = honk::octags::parse_tag_arg)]
    tag: Option<honk::octags::TagArg>,

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
            let (names, confirm) = honk::swarm_mcp::parse_enable_names(&args.mcp)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
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
fn chat_tag_settings(args: &ChatArgs) -> Option<honk::octags::TagAction> {
    chat_tag_settings_with_env(args, |k| std::env::var(k).ok())
}

fn chat_tag_settings_with_env(
    args: &ChatArgs,
    env_get: impl Fn(&str) -> Option<String>,
) -> Option<honk::octags::TagAction> {
    if !args.new {
        return None;
    }
    match &args.tag {
        Some(honk::octags::TagArg::Explicit(t)) => {
            let bin = env_get(honk::octags::BIN_ENV)
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| honk::octags::DEFAULT_BIN.to_string());
            Some(honk::octags::TagAction::Explicit(
                honk::octags::TagSettings {
                    tag: t.clone(),
                    bin,
                    timeout: honk::octags::DEFAULT_TIMEOUT,
                },
            ))
        }
        Some(honk::octags::TagArg::NoInherit) => None,
        None => {
            let launcher = honk::octags::resolve_launcher_from(&env_get)?;
            let custom_bin = env_get(honk::octags::BIN_ENV)
                .filter(|b| !b.is_empty())
                .is_some();
            let bin = env_get(honk::octags::BIN_ENV)
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| honk::octags::DEFAULT_BIN.to_string());
            Some(honk::octags::TagAction::Inherit(
                honk::octags::InheritSettings {
                    launcher,
                    bin,
                    custom_bin,
                    which_timeout: honk::octags::DEFAULT_WHICH_TIMEOUT,
                    set_timeout: honk::octags::DEFAULT_TIMEOUT,
                },
            ))
        }
    }
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
            opts.no_swarm = args.no_swarm;
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
        Commands::Launch(args) => {
            let code = run_launch(args).await;
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(code);
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
            no_swarm: false,
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
            no_swarm: false,
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
                match tag {
                    honk::octags::TagAction::Explicit(s) => assert_eq!(s.tag, "honk"),
                    _ => panic!("expected explicit tag"),
                }
            }
            _ => panic!("expected chat command"),
        }
        let cli = Cli::try_parse_from(["honk", "chat", "--new"]).unwrap();
        match cli.command {
            Commands::Chat(args) => {
                // Without launcher in env, returns None
                let no_env = |_: &str| None;
                assert!(chat_tag_settings_with_env(&args, no_env).is_none());

                // With OPENCODE_SESSION_ID in env, inherits
                let opencode_env = |k: &str| match k {
                    honk::octags::OPENCODE_SESSION_ID_ENV => Some("ses_123".to_string()),
                    _ => None,
                };
                match chat_tag_settings_with_env(&args, opencode_env).unwrap() {
                    honk::octags::TagAction::Inherit(s) => assert_eq!(s.launcher, "ses_123"),
                    _ => panic!("expected inherit"),
                }

                // With AGENT_SESSION_ID in env, inherits with goose: prefix
                let goose_env = |k: &str| match k {
                    honk::octags::AGENT_SESSION_ID_ENV => Some("456".to_string()),
                    _ => None,
                };
                match chat_tag_settings_with_env(&args, goose_env).unwrap() {
                    honk::octags::TagAction::Inherit(s) => assert_eq!(s.launcher, "goose:456"),
                    _ => panic!("expected inherit"),
                }
            }
            _ => panic!("expected chat command"),
        }

        // Explicit tag wins over launcher env
        let cli = Cli::try_parse_from(["honk", "chat", "--new", "--tag", "honk"]).unwrap();
        match cli.command {
            Commands::Chat(args) => {
                let env = |k: &str| match k {
                    honk::octags::OPENCODE_SESSION_ID_ENV => Some("ses_123".to_string()),
                    _ => None,
                };
                match chat_tag_settings_with_env(&args, env).unwrap() {
                    honk::octags::TagAction::Explicit(s) => assert_eq!(s.tag, "honk"),
                    _ => panic!("expected explicit"),
                }
            }
            _ => panic!("expected chat command"),
        }

        // --tag auto opts out even with launcher env
        let cli = Cli::try_parse_from(["honk", "chat", "--new", "--tag", "auto"]).unwrap();
        match cli.command {
            Commands::Chat(args) => {
                let env = |k: &str| match k {
                    honk::octags::OPENCODE_SESSION_ID_ENV => Some("ses_123".to_string()),
                    _ => None,
                };
                assert!(chat_tag_settings_with_env(&args, env).is_none());
            }
            _ => panic!("expected chat command"),
        }

        // Attach never tags even with launcher env
        let cli = Cli::try_parse_from(["honk", "chat", "20261024_1"]).unwrap();
        match cli.command {
            Commands::Chat(args) => {
                let env = |k: &str| match k {
                    honk::octags::OPENCODE_SESSION_ID_ENV => Some("ses_123".to_string()),
                    _ => None,
                };
                assert!(chat_tag_settings_with_env(&args, env).is_none());
            }
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_parse_no_swarm_flag() {
        let cli = Cli::try_parse_from(["honk", "chat", "--new", "--no-swarm"]).unwrap();
        match cli.command {
            Commands::Chat(args) => assert!(args.no_swarm),
            _ => panic!("expected chat command"),
        }
        let cli = Cli::try_parse_from(["honk", "chat", "--new"]).unwrap();
        match cli.command {
            Commands::Chat(args) => assert!(!args.no_swarm),
            _ => panic!("expected chat command"),
        }
    }

    #[test]
    fn test_cli_tag_requires_new_and_a_valid_tag() {
        // Attaching does not tag: the session already has whatever tag it has.
        assert!(Cli::try_parse_from(["honk", "chat", "20261020_3", "--tag", "honk"]).is_err());
        assert!(Cli::try_parse_from(["honk", "chat", "--tag", "honk"]).is_err());
        // A bad tag is refused before any session exists.
        for bad in ["auto:x", "AUTO:x", "has space", "-x", ""] {
            assert!(
                Cli::try_parse_from(["honk", "chat", "--new", "--tag", bad]).is_err(),
                "{bad:?}"
            );
        }
        // Bare auto opts out.
        for auto in ["auto", "AUTO", "Auto"] {
            let cli = Cli::try_parse_from(["honk", "chat", "--new", "--tag", auto]).unwrap();
            match cli.command {
                Commands::Chat(args) => {
                    assert_eq!(args.tag, Some(honk::octags::TagArg::NoInherit));
                    assert!(chat_tag_settings(&args).is_none());
                }
                _ => panic!("expected chat command"),
            }
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
    fn test_cli_parse_mcp_aligned_edge_cases() {
        let temp = tempfile::tempdir().unwrap();

        // "" is rejected
        let cli_empty = Cli::try_parse_from([
            "honk",
            "chat",
            "--new",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--mcp",
            "",
        ])
        .unwrap();
        match cli_empty.command {
            Commands::Chat(args) => {
                assert!(resolve_chat_target(&args).is_err());
            }
            _ => panic!("expected chat command"),
        }

        // "!" is rejected
        let cli_bang = Cli::try_parse_from([
            "honk",
            "chat",
            "--new",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--mcp",
            "!",
        ])
        .unwrap();
        match cli_bang.command {
            Commands::Chat(args) => {
                assert!(resolve_chat_target(&args).is_err());
            }
            _ => panic!("expected chat command"),
        }

        // "slack!!" confirms slack
        let cli_double_bang = Cli::try_parse_from([
            "honk",
            "chat",
            "--new",
            "--cwd",
            temp.path().to_str().unwrap(),
            "--mcp",
            "slack!!",
        ])
        .unwrap();
        match cli_double_bang.command {
            Commands::Chat(args) => {
                let target = resolve_chat_target(&args).unwrap();
                match target {
                    honk::chat::driver::Target::New { mcp, .. } => {
                        assert_eq!(
                            mcp,
                            Some(honk::chat::driver::McpLaunchConfig {
                                names: vec!["slack".into()],
                                confirm: vec!["slack".into()],
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

    fn launch(argv: &[&str]) -> Result<LaunchArgs, clap::Error> {
        let mut full = vec!["honk", "launch"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full).map(|c| match c.command {
            Commands::Launch(a) => a,
            _ => panic!("expected launch command"),
        })
    }

    fn launch_env(k: &str) -> Option<String> {
        match k {
            honk::octags::AGENT_SESSION_ID_ENV => Some("20261104_1".into()),
            honk::octags::BIN_ENV => Some("/bin/tags".into()),
            _ => None,
        }
    }

    #[test]
    fn test_launch_parses_dir_prompt_and_flags() {
        let temp = tempfile::tempdir().unwrap();
        let d = temp.path().to_str().unwrap();
        let a = launch(&[
            d,
            "--tag",
            "honk",
            "--title",
            " Fix it ",
            "--secret",
            "s",
            "--pigeon-url",
            "http://p",
            "do it",
        ])
        .unwrap();
        let o = launch_opts(a, launch_env, || unreachable!()).unwrap();
        assert_eq!(o.dir, temp.path().canonicalize().unwrap());
        assert_eq!(o.prompt, "do it");
        assert_eq!(o.title.as_deref(), Some("Fix it"));
        assert_eq!(o.tag, honk::launch::TagRequest::Explicit("honk".into()));
        assert_eq!(o.launcher.as_deref(), Some("goose:20261104_1"));
        assert_eq!(o.tags_bin, "/bin/tags");
        assert_eq!(o.pigeon.url, "http://p");
        assert_eq!(o.secret, "s");
    }

    #[test]
    fn test_launch_tag_defaults_and_auto() {
        let temp = tempfile::tempdir().unwrap();
        let d = temp.path().to_str().unwrap();
        let o = launch_opts(
            launch(&[d, "--secret", "s", "p"]).unwrap(),
            launch_env,
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(o.tag, honk::launch::TagRequest::Inherit);
        let o = launch_opts(
            launch(&[d, "--tag", "auto", "--secret", "s", "p"]).unwrap(),
            launch_env,
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(o.tag, honk::launch::TagRequest::Off);
        assert!(launch(&[d, "--tag", "auto:x", "p"]).is_err());
    }

    #[test]
    fn test_launch_prompt_from_stdin_and_refusals() {
        let temp = tempfile::tempdir().unwrap();
        let d = temp.path().to_str().unwrap();
        let o = launch_opts(
            launch(&[d, "--secret", "s", "-"]).unwrap(),
            launch_env,
            || Ok("from stdin\n".into()),
        )
        .unwrap();
        assert_eq!(o.prompt, "from stdin\n");

        assert!(launch(&[d]).is_err(), "prompt is required");
        let err = |argv: &[&str], stdin: &'static str| {
            launch_opts(launch(argv).unwrap(), launch_env, || Ok(stdin.into()))
                .unwrap_err()
                .to_string()
        };
        assert!(err(&[d, "--secret", "s", "  "], "").contains("prompt is empty"));
        assert!(err(&[d, "--secret", "s", "-"], "\n").contains("prompt is empty"));
        assert!(err(&[d, "--secret", "s", "--title", " ", "p"], "").contains("--title"));
        assert!(err(&["/nonexistent_xyz_123", "--secret", "s", "p"], "").contains("invalid dir"));
        let f = temp.path().join("file");
        std::fs::write(&f, "").unwrap();
        assert!(err(&[f.to_str().unwrap(), "--secret", "s", "p"], "").contains("not a directory"));
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
