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
    /// honkd WebSocket or HTTP endpoint
    #[arg(long, default_value = "http://127.0.0.1:4280/acp", env = "HONK_URL")]
    url: String,
    /// Secret auth token (defaults to $HONK_SECRET or state file)
    #[arg(long)]
    secret: Option<String>,
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
            honk::run(opts, stdout()).await?;
        }
        Commands::List(args) => {
            let secret = resolve_secret(args.secret.as_deref())?;
            let mut client = Client::connect(&args.url, &secret).await?;
            let sessions = client.list_sessions().await?;
            for s in sessions {
                println!("{}", s.format_line());
            }
        }
        Commands::Chat(args) => {
            let secret = resolve_secret(args.secret.as_deref())?;
            let target = if args.new {
                let cwd = match args.cwd {
                    Some(c) => c,
                    None => std::env::current_dir()?,
                };
                let extensions = if args.ext.is_empty() {
                    None
                } else {
                    Some(args.ext)
                };
                honk::chat::driver::Target::New { cwd, extensions }
            } else {
                honk::chat::driver::Target::Attach(args.session.unwrap())
            };

            let session_id = match &target {
                honk::chat::driver::Target::Attach(id) => id.clone(),
                honk::chat::driver::Target::New { .. } => String::new(),
            };

            let opts = honk::chat::driver::DriverOpts::new(args.url, secret, target);
            let (handle, ui_rx) = honk::chat::driver::start(opts).await?;
            let app = honk::tui::App::new(session_id, handle.cmd_tx());
            honk::tui::app::run_app(app, ui_rx, handle).await?;
        }
    }
    Ok(())
}
