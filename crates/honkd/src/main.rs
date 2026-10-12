use clap::Parser;
use honkd::config::Args;
use honkd::downstream;
use honkd::hub::Hub;
use honkd::state::StateStore;
use std::io::{Read, Write};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 1. Initialize JSON tracing. No request URIs or headers are ever logged.
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();

    // 2. Refuse a non-loopback listen, then call guard_url_with(upstream, allow_production_upstream)
    if !args.listen.ip().is_loopback() {
        eprintln!(
            "honkd: refusing to bind non-loopback listen address: {}",
            args.listen
        );
        std::process::exit(1);
    }

    if let Err(e) = honk_upstream::guard_url_with(&args.upstream, args.allow_production_upstream) {
        eprintln!("honkd: {e:#}");
        std::process::exit(1);
    }

    // 3. StateStore::open (it takes the lock and checks the upstream URL)
    let state_dir = args.resolve_state_dir();
    let state_store =
        match StateStore::open(&state_dir, Some(&args.upstream), args.discard_corrupt_state) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("honkd: {e:#}");
                std::process::exit(1);
            }
        };

    // 4. Load or create the downstream secret at <state>/secret
    let secret_path = state_dir.join("secret");
    let downstream_secret = if secret_path.exists() {
        match std::fs::read_to_string(&secret_path) {
            Ok(s) => {
                let trimmed = s.trim().to_string();
                if trimmed.len() < 32 {
                    eprintln!(
                        "honkd: downstream secret file {} is invalid (empty or shorter than 32 characters)",
                        secret_path.display()
                    );
                    std::process::exit(1);
                }
                trimmed
            }
            Err(e) => {
                eprintln!("honkd: failed to read downstream secret: {e:#}");
                std::process::exit(1);
            }
        }
    } else {
        let mut bytes = [0u8; 32];
        let mut f = match std::fs::File::open("/dev/urandom") {
            Ok(f) => f,
            Err(e) => {
                eprintln!("honkd: failed to open /dev/urandom: {e:#}");
                std::process::exit(1);
            }
        };
        if let Err(e) = f.read_exact(&mut bytes) {
            eprintln!("honkd: failed to read /dev/urandom: {e:#}");
            std::process::exit(1);
        }
        let hex_str = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&secret_path) {
            Ok(mut out) => {
                if let Err(e) = out
                    .write_all(hex_str.as_bytes())
                    .and_then(|_| out.write_all(b"\n"))
                    .and_then(|_| out.sync_all())
                {
                    eprintln!("honkd: failed to write downstream secret: {e:#}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("honkd: failed to create downstream secret: {e:#}");
                std::process::exit(1);
            }
        }
        hex_str
    };

    // 5. Bind the listener
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "honkd: failed to bind listen address {}: {e:#}",
                args.listen
            );
            std::process::exit(1);
        }
    };
    let bound_addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("honkd: failed to get local address: {e:#}");
            std::process::exit(1);
        }
    };

    // 6. Hub::start
    let upstream_secret = match std::fs::read_to_string(&args.upstream_secret_file) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.len() < 32 {
                eprintln!(
                    "honkd: upstream secret file {} is invalid (empty or shorter than 32 characters)",
                    args.upstream_secret_file.display()
                );
                std::process::exit(1);
            }
            trimmed
        }
        Err(e) => {
            eprintln!(
                "honkd: failed to read upstream secret file {}: {e:#}",
                args.upstream_secret_file.display()
            );
            std::process::exit(1);
        }
    };

    let args_arc = Arc::new(args);
    let hub = match Hub::start(args_arc, state_store, upstream_secret).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("honkd: {e:#}");
            std::process::exit(1);
        }
    };

    // 7. Print honkd listening on <addr> to stdout, then serve
    println!("honkd listening on {bound_addr}");
    let _ = std::io::stdout().flush();

    let app = downstream::router(hub, downstream_secret);

    // 8. SIGTERM/SIGINT: stop accepting and exit, with no upstream calls
    let server = axum::serve(listener, app);
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
    };

    if let Err(e) = server.with_graceful_shutdown(shutdown).await {
        eprintln!("honkd: server error: {e:#}");
        std::process::exit(1);
    }
}
