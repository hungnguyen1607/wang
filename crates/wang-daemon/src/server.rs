//! The daemon side: a [`Hub`] (agent + sessions) and [`Channel`]s that feed it.
//! Channels: a Unix socket for `wang chat`, and Telegram (`telegram.rs`).

use std::collections::HashMap;
use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::agent::{Agent, Conversation};
use crate::model::ModelClient;
use crate::protocol::{ClientMsg, ServerMsg};

/// Shared state behind every channel.
pub struct Hub<C> {
    pub agent: Agent<C>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Conversation>>>>,
}

impl<C: ModelClient> Hub<C> {
    pub fn new(agent: Agent<C>) -> Self {
        Self { agent, sessions: Mutex::new(HashMap::new()) }
    }

    pub async fn session(&self, key: &str) -> Arc<Mutex<Conversation>> {
        self.sessions
            .lock()
            .await
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Conversation::new(key))))
            .clone()
    }

    pub async fn reset(&self, key: &str) {
        self.sessions.lock().await.remove(key);
    }
}

/// A way for a person to reach the agent (terminal socket, Telegram).
pub trait Channel<C: ModelClient>: Send + 'static {
    fn name(&self) -> &'static str;
    fn serve(self, hub: Arc<Hub<C>>) -> impl Future<Output = Result<()>> + Send;
}

pub struct UnixSocketChannel {
    pub path: PathBuf,
    listener: UnixListener,
}

impl UnixSocketChannel {
    /// Bind first, so a failed start never touches another daemon's socket.
    pub async fn bind(path: PathBuf) -> Result<Self> {
        let listener = bind_socket(&path).await?;
        Ok(Self { path, listener })
    }
}

fn my_uid() -> Result<u32> {
    Ok(std::fs::metadata("/proc/self")?.uid())
}

/// Bind the socket: refuse if a live daemon owns it, replace a stale file, chmod 600.
pub async fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("another wang daemon is already listening on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl<C: ModelClient> Channel<C> for UnixSocketChannel {
    fn name(&self) -> &'static str {
        "unix-socket"
    }

    async fn serve(self, hub: Arc<Hub<C>>) -> Result<()> {
        let listener = self.listener;
        let uid = my_uid()?;
        tracing::info!("listening on {}", self.path.display());
        loop {
            let (stream, _) = listener.accept().await?;
            // Defense in depth on top of 0600: only our own user may drive the agent.
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == uid => {}
                Ok(cred) => {
                    tracing::warn!("rejected connection from uid {}", cred.uid());
                    continue;
                }
                Err(e) => {
                    tracing::warn!("rejected connection: no peer credentials ({e})");
                    continue;
                }
            }
            let hub = hub.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, hub).await {
                    tracing::warn!("connection error: {e:#}");
                }
            });
        }
    }
}

async fn send(w: &mut BufWriter<tokio::net::unix::OwnedWriteHalf>, msg: &ServerMsg) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

async fn handle_conn<C: ModelClient>(stream: UnixStream, hub: Arc<Hub<C>>) -> Result<()> {
    let (r, w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let mut w = BufWriter::new(w);
    while let Some(line) = lines.next_line().await? {
        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                send(&mut w, &ServerMsg::Error { message: format!("bad request: {e}") }).await?;
                continue;
            }
        };
        match msg {
            ClientMsg::New { session } => {
                hub.reset(&session).await;
                send(&mut w, &ServerMsg::Ok).await?;
            }
            ClientMsg::Approve { id, allow } => {
                let ok = hub.agent.approvals.as_ref().is_some_and(|a| a.resolve(&id, allow));
                let reply = if ok { ServerMsg::Ok } else { ServerMsg::Error { message: format!("no pending approval {id}") } };
                send(&mut w, &reply).await?;
            }
            ClientMsg::Message { session, text } => {
                let conv = hub.session(&session).await;
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
                let hub2 = hub.clone();
                let turn = async move {
                    let mut conv = conv.lock().await;
                    let emit = |ev: ServerMsg| {
                        let _ = tx.send(ev);
                    };
                    hub2.agent.run_turn(&mut conv, "cli", &text, &emit).await
                    // tx dropped here, which ends the forwarding loop below
                };
                let forward = async {
                    while let Some(ev) = rx.recv().await {
                        if send(&mut w, &ev).await.is_err() {
                            break; // client went away; let the task finish anyway
                        }
                    }
                };
                let (result, ()) = tokio::join!(turn, forward);
                let last = match result {
                    Ok(r) => ServerMsg::Done { task_id: r.task_id, footer: r.footer() },
                    Err(e) => {
                        tracing::error!("task failed: {e:#}");
                        ServerMsg::Error { message: format!("{e:#}") }
                    }
                };
                send(&mut w, &last).await?;
            }
        }
    }
    Ok(())
}

/// Client side, used by `wang chat`.
pub struct Client {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    pub async fn connect(path: &Path) -> Result<Self> {
        let s = UnixStream::connect(path).await?;
        let (r, w) = s.into_split();
        Ok(Self { lines: BufReader::new(r).lines(), w })
    }

    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<ServerMsg> {
        let line = self.lines.next_line().await?.context("daemon closed the connection")?;
        Ok(serde_json::from_str(&line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::agent;
    use crate::model::testing::{Mock, resp};
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn socket_roundtrip_streams_events() {
        let dir = std::env::temp_dir().join(format!("wang-sock-{}", std::process::id()));
        let path = dir.join("wang.sock");
        let a = agent(Mock::new(vec![
            resp(json!([{"type": "text", "text": "Looking."}, {"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "echo hi"}}]), "tool_use", 0),
            resp(json!([{"type": "text", "text": "It said hi."}]), "end_turn", 0),
        ]));
        let hub = Arc::new(Hub::new(a));
        let ch = UnixSocketChannel::bind(path.clone()).await.unwrap();
        let server = tokio::spawn(ch.serve(hub.clone()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let mut c = Client::connect(&path).await.unwrap();
        c.send(&ClientMsg::Message { session: "t".into(), text: "say hi".into() }).await.unwrap();
        let mut got = vec![];
        loop {
            let m = c.recv().await.unwrap();
            let end = matches!(m, ServerMsg::Done { .. } | ServerMsg::Error { .. });
            got.push(m);
            if end {
                break;
            }
        }
        assert_eq!(got[0], ServerMsg::Text { delta: "Looking.".into() });
        assert!(matches!(&got[1], ServerMsg::ToolStart { name, detail } if name == "shell" && detail == "echo hi"));
        assert!(matches!(&got[2], ServerMsg::ToolEnd { ok: true, .. }));
        assert_eq!(got[3], ServerMsg::Text { delta: "It said hi.".into() });
        assert!(matches!(&got[4], ServerMsg::Done { footer, .. } if footer.contains("2 calls")));

        // session kept across messages, reset by New
        assert_eq!(hub.session("t").await.lock().await.messages.len(), 4);
        c.send(&ClientMsg::New { session: "t".into() }).await.unwrap();
        assert_eq!(c.recv().await.unwrap(), ServerMsg::Ok);
        assert!(hub.session("t").await.lock().await.messages.is_empty());

        // second daemon on the same socket is refused
        assert!(bind_socket(&path).await.is_err());

        server.abort();
        std::fs::remove_dir_all(dir).ok();
    }
}
