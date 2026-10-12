use clap::{Args, Parser, Subcommand};
use honk::{Client, TailOpts, resolve_secret};
use std::io::stdout;

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
    }
    Ok(())
}
