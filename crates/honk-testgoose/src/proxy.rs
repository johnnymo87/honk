//! TCP forwarding proxy for simulating ungraceful network drops and half-open streams.
//!
//! Ported from M0 disconnect probes P2d and P2f (`honk-upstream/tests/common/proxy.rs`).
//! - Cites M0 probe P2d: `freeze()` stops reading and writing on currently open sockets
//!   while holding them open without sending TCP FIN, simulating an ungraceful half-open stream.
//! - Cites M0 probe P2f: new connections continue to pass through while earlier streams
//!   are frozen, allowing takeover reattachment over a fresh connection.
//! - Cites M0 probe P2d: `release()` drops frozen sockets, sending TCP FIN to allow TCP teardown.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

pub struct TcpProxy {
    addr: SocketAddr,
    freeze_tx: broadcast::Sender<()>,
    drop_on_prompt: Arc<AtomicBool>,
    held_sockets: Arc<Mutex<Vec<TcpStream>>>,
    server_task: tokio::task::JoinHandle<()>,
}

impl TcpProxy {
    /// Starts the proxy forwarding to `upstream_addr`.
    pub async fn start(upstream_addr: SocketAddr) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (freeze_tx, _) = broadcast::channel(16);
        let drop_on_prompt = Arc::new(AtomicBool::new(false));
        let held_sockets = Arc::new(Mutex::new(Vec::new()));

        let freeze_tx_clone = freeze_tx.clone();
        let drop_prompt_clone = drop_on_prompt.clone();
        let held_clone = held_sockets.clone();

        let server_task = tokio::spawn(async move {
            loop {
                let (mut client_sock, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };

                let mut target_sock = match TcpStream::connect(upstream_addr).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[TcpProxy] failed to connect to {upstream_addr}: {e}");
                        continue;
                    }
                };

                // Each active connection gets its own receiver subscribed at connection time.
                // Connections established AFTER a freeze() call will NOT receive past freeze broadcasts,
                // allowing new connections to pass through freely (M0 probe P2f).
                let mut freeze_rx = freeze_tx_clone.subscribe();
                let drop_prompt = drop_prompt_clone.clone();
                let held = held_clone.clone();

                tokio::spawn(async move {
                    if drop_prompt.load(Ordering::SeqCst) {
                        let (mut c_read, mut c_write) = client_sock.into_split();
                        let (mut t_read, mut t_write) = target_sock.into_split();
                        let mut c_buf = vec![0u8; 8192];
                        let mut t_buf = vec![0u8; 8192];
                        loop {
                            tokio::select! {
                                res = c_read.read(&mut c_buf) => {
                                    match res {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            let _ = t_write.write_all(&c_buf[..n]).await;
                                            let _ = t_write.flush().await;
                                            let has_prompt = c_buf[..n].windows(b"session/prompt".len()).any(|w| w == b"session/prompt");
                                            if has_prompt {
                                                drop(c_read);
                                                drop(c_write);
                                                tokio::spawn(async move {
                                                    let _tw = t_write;
                                                    let mut discard = vec![0u8; 1024];
                                                    while let Ok(n) = t_read.read(&mut discard).await {
                                                        if n == 0 {
                                                            break;
                                                        }
                                                    }
                                                });
                                                break;
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                res = t_read.read(&mut t_buf) => {
                                    match res {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            if c_write.write_all(&t_buf[..n]).await.is_err() {
                                                break;
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                    } else {
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client_sock, &mut target_sock) => {}
                            _ = freeze_rx.recv() => {
                                // Freeze: hold sockets open without reading or writing (half-open, no FIN; M0 probe P2d)
                                let mut guard = held.lock().unwrap();
                                guard.push(client_sock);
                                guard.push(target_sock);
                            }
                        }
                    }
                });
            }
        });

        Ok(Self {
            addr,
            freeze_tx,
            drop_on_prompt,
            held_sockets,
            server_task,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// When set to true, the proxy will read the prompt POST from the client,
    /// forward it to the target, and then immediately drop the client connection,
    /// simulating a network drop / TCP reset after the request body was received by upstream.
    pub fn set_drop_on_prompt(&self, drop: bool) {
        self.drop_on_prompt.store(drop, Ordering::SeqCst);
    }

    /// Stops reading and writing on currently open sockets while keeping them open
    /// without sending TCP FIN (half-open).
    ///
    /// Cites M0 probe P2d: simulates half-open client failure where server continues
    /// holding stream lease and returning 409 Conflict until TCP teardown or takeover.
    pub fn freeze(&self) {
        let _ = self.freeze_tx.send(());
    }

    /// Drops all frozen sockets, sending TCP FIN.
    ///
    /// Cites M0 probe P2d: verifies subscriber recovery once TCP connection teardown occurs.
    pub async fn release(&self) {
        let mut guard = self.held_sockets.lock().unwrap();
        guard.clear();
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        self.server_task.abort();
        if let Ok(mut guard) = self.held_sockets.lock() {
            guard.clear();
        }
    }
}
