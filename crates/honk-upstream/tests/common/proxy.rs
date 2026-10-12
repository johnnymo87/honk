#![allow(dead_code)]

use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, watch};

pub async fn wait_for_freeze(f_rx: &mut watch::Receiver<bool>) {
    loop {
        if *f_rx.borrow() {
            return;
        }
        if f_rx.changed().await.is_err() {
            return;
        }
    }
}

pub struct TcpProxy {
    pub port: u16,
    freeze_tx: watch::Sender<bool>,
    close_tx: broadcast::Sender<()>,
    held_sockets: Arc<Mutex<Vec<TcpStream>>>,
    server_task: tokio::task::JoinHandle<()>,
}

impl TcpProxy {
    pub async fn start(target_port: u16) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let (freeze_tx, _) = watch::channel(false);
        let (close_tx, _) = broadcast::channel(16);
        let held_sockets = Arc::new(Mutex::new(Vec::new()));

        let freeze_rx = freeze_tx.subscribe();
        let close_tx_clone = close_tx.clone();
        let held_clone = held_sockets.clone();

        let server_task = tokio::spawn(async move {
            let mut close_rx = close_tx_clone.subscribe();
            loop {
                tokio::select! {
                    accept_res = listener.accept() => {
                        let (mut client_sock, _) = match accept_res {
                            Ok(pair) => pair,
                            Err(_) => break,
                        };
                        let target_addr = format!("127.0.0.1:{target_port}");
                        let mut target_sock = match TcpStream::connect(&target_addr).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("[Proxy] failed to connect to target {target_addr}: {e}");
                                continue;
                            }
                        };

                        let mut f_rx = freeze_rx.clone();
                        let mut c_rx = close_tx_clone.subscribe();
                        let held = held_clone.clone();

                        tokio::spawn(async move {
                            tokio::select! {
                                _ = tokio::io::copy_bidirectional(&mut client_sock, &mut target_sock) => {}
                                _ = wait_for_freeze(&mut f_rx) => {
                                    // Freeze: keep sockets open without reading or writing
                                    let mut guard = held.lock().await;
                                    guard.push(client_sock);
                                    guard.push(target_sock);
                                    drop(guard);
                                    let _ = c_rx.recv().await;
                                }
                                _ = c_rx.recv() => {}
                            }
                        });
                    }
                    _ = close_rx.recv() => {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            port,
            freeze_tx,
            close_tx,
            held_sockets,
            server_task,
        })
    }

    pub fn freeze(&self) {
        let _ = self.freeze_tx.send(true);
    }

    pub async fn close(&self) {
        let _ = self.close_tx.send(());
        self.server_task.abort();
        let mut guard = self.held_sockets.lock().await;
        guard.clear();
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        let _ = self.close_tx.send(());
        self.server_task.abort();
        if let Ok(mut guard) = self.held_sockets.try_lock() {
            guard.clear();
        }
    }
}
