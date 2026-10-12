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
    #[arg(long, default_value_t = 16, hide = true)]
    pub max_sessions: usize,
    #[arg(long, default_value_t = 20, hide = true)]
    pub stream_dead_secs: u64,
    #[arg(long, default_value_t = 2000, hide = true)]
    pub gap_synthesis_grace_ms: u64,
    #[arg(long, default_value_t = 5, hide = true)]
    pub probe_timeout_secs: u64,
    #[arg(long, default_value_t = 30, hide = true)]
    pub ambiguous_inflight_timeout_secs: u64,
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
