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
    #[arg(long = "ext", requires = "new")]
    ext: Vec<String>,

    /// Cap model calls per turn (positive integer)
    #[arg(long = "max-turns", env = "HONK_MAX_TURNS", value_parser = parse_max_turns)]
    max_turns: Option<u32>,

    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,

    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
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
        Ok(honk::chat::driver::Target::New { cwd, extensions })
    } else {
        Ok(honk::chat::driver::Target::Attach(
            args.session.clone().unwrap(),
        ))
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
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            use std::io::Write;
            for s in sessions {
                if let Err(e) = writeln!(handle, "{}", s.format_line()) {
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

            let mut opts = honk::chat::driver::DriverOpts::new(args.url, secret, target);
            opts.max_turns = args.max_turns;
            let (handle, ui_rx) = honk::chat::driver::start(opts).await?;
            let app = honk::tui::App::new(session_id, handle.cmd_tx());
            honk::tui::app::run_app(app, ui_rx, handle).await?;
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
            max_turns: None,
            url: "http://127.0.0.1:4280/acp".into(),
            secret: None,
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
            max_turns: None,
            url: "http://127.0.0.1:4280/acp".into(),
            secret: None,
        };
        let target = resolve_chat_target(&args).unwrap();
        match target {
            honk::chat::driver::Target::New { cwd, extensions } => {
                assert_eq!(cwd, temp.path().canonicalize().unwrap());
                assert_eq!(extensions, Some(vec!["dev".into()]));
            }
            _ => panic!("expected Target::New"),
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
