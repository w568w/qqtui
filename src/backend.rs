use crate::model::BridgeEvent;
use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc, oneshot, watch},
};

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
struct Request {
    id: u64,
    pending: Pending,
    writing: bool,
    alive: Arc<AtomicBool>,
    close: watch::Sender<()>,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
        if self.writing {
            self.alive.store(false, Ordering::Release);
            self.close.send_replace(());
        }
    }
}
pub enum Event {
    Connection { id: uuid::Uuid, event: Box<Event> },
    Native(Box<BridgeEvent>),
    Disconnected(String),
}

#[derive(Clone)]
pub struct Backend {
    id: uuid::Uuid,
    alive: Arc<AtomicBool>,
    _lifetime: watch::Sender<()>,
    writer: Arc<Mutex<crate::platform::LocalWriteHalf>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}
impl Backend {
    pub async fn connect(path: &Path, events: mpsc::Sender<Event>) -> Result<Self> {
        let stream = crate::platform::connect(path)
            .await
            .context("连接 QQ 宿主失败，请运行 qqtui backend start")?;
        let (reader, writer) = tokio::io::split(stream);
        let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (lifetime, mut released) = watch::channel(());
        let id = uuid::Uuid::new_v4();
        let alive = Arc::new(AtomicBool::new(true));
        let client = Self {
            id,
            alive: alive.clone(),
            _lifetime: lifetime,
            writer: Arc::new(Mutex::new(writer)),
            pending: pending.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        let writer = client.writer.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let error = loop {
                let mut frame = Vec::new();
                // read_until is bounded by the protocol limit even for a peer
                // that never terminates a frame.
                let mut limited = (&mut reader).take(8 * 1024 * 1024 + 1);
                use tokio::io::AsyncReadExt;
                let read = tokio::select! {
                    _=released.changed()=>break "本地连接已关闭".to_owned(),
                    result=limited.read_until(b'\n',&mut frame)=>result,
                };
                match read {
                    Ok(0) => break "QQ 宿主已断开".to_owned(),
                    Ok(_) if frame.len() > 8 * 1024 * 1024 => {
                        break "桥接帧超过大小限制".to_owned();
                    }
                    Err(e) => break e.to_string(),
                    _ => {}
                }
                let value: Value = match serde_json::from_slice(&frame) {
                    Ok(v) => v,
                    Err(e) => break e.to_string(),
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some(reply) = pending.lock().unwrap().remove(&id) {
                        let result = if let Some(error) = value.get("error") {
                            Err(anyhow!(
                                "{}",
                                error["message"].as_str().unwrap_or("QQ 请求失败")
                            ))
                        } else {
                            Ok(value["result"].clone())
                        };
                        let _ = reply.send(result);
                    }
                } else if value["method"] == "event" {
                    match serde_json::from_value(value["params"].clone()) {
                        Ok(event) => {
                            if events
                                .try_send(Event::Connection {
                                    id,
                                    event: Box::new(Event::Native(Box::new(event))),
                                })
                                .is_err()
                            {
                                break "事件队列已满，需要重新同步".to_owned();
                            }
                        }
                        Err(e) => break e.to_string(),
                    }
                }
            };
            alive.store(false, Ordering::Release);
            let _ = writer.lock().await.shutdown().await;
            for (_, reply) in pending.lock().unwrap().drain() {
                let _ = reply.send(Err(anyhow!(error.clone())));
            }
            let _ = events
                .send(Event::Connection {
                    id,
                    event: Box::new(Event::Disconnected(error)),
                })
                .await;
        });
        Ok(client)
    }
    pub fn id(&self) -> uuid::Uuid {
        self.id
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        if !self.is_alive() {
            return Err(anyhow!("QQ 宿主连接已关闭"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = serde_json::to_vec(
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )?;
        frame.push(b'\n');
        anyhow::ensure!(frame.len() <= 8 * 1024 * 1024, "桥接请求超过大小限制");
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut request = Request {
            id,
            pending: self.pending.clone(),
            writing: false,
            alive: self.alive.clone(),
            close: self._lifetime.clone(),
        };
        let seconds = if method == "media.resolve" { 125 } else { 20 };
        let value = tokio::time::timeout(Duration::from_secs(seconds), async {
            let mut writer = self.writer.lock().await;
            anyhow::ensure!(self.is_alive(), "QQ 宿主连接已关闭");
            request.writing = true;
            writer.write_all(&frame).await?;
            request.writing = false;
            drop(writer);
            rx.await.context("桥接连接关闭")?
        })
        .await
        .context(if method == "messages.send" {
            "发送请求超时；结果需要核对"
        } else {
            "请求超时，请重试"
        })??;
        serde_json::from_value(value).context("桥接响应格式不匹配")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_rpc_responses_are_matched_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut requests = vec![];
            for _ in 0..2 {
                requests.push(
                    serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap())
                        .unwrap(),
                );
            }
            for r in requests.into_iter().rev() {
                w.write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":r["id"],"result":r["method"]})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            }
        });
        let (tx, _) = mpsc::channel(8);
        let b = Backend::connect(&socket, tx).await.unwrap();
        let (a, c) = tokio::join!(
            b.call::<String>("first", json!({})),
            b.call::<String>("second", json!({}))
        );
        assert_eq!(a.unwrap(), "first");
        assert_eq!(c.unwrap(), "second");
    }
    #[tokio::test]
    async fn disconnect_fails_pending_rpc_without_waiting_for_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut byte = [0u8; 1];
            use tokio::io::AsyncReadExt;
            let _ = stream.read(&mut byte).await;
        });
        let (tx, _) = mpsc::channel(8);
        let b = Backend::connect(&socket, tx).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            b.call::<Value>("pending", json!({})),
        )
        .await;
        assert!(result.unwrap().is_err());
    }
    #[tokio::test]
    async fn dropping_the_last_client_closes_the_socket_even_without_a_remote_disconnect() {
        use tokio::io::AsyncReadExt;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("lifetime.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (events, _) = mpsc::channel(4);
        let client = Backend::connect(&socket, events).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        let other = client.clone();
        drop(client);
        let mut byte = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.read(&mut byte))
                .await
                .is_err()
        );
        drop(other);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), server.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}

#[cfg(all(test, unix))]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_during_a_partial_frame_cleans_registration_and_closes_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (events, _) = mpsc::channel(4);
        let backend = Backend::connect(&path, events).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(80),
                backend.call::<Value>("large", json!({"body":"x".repeat(4*1024*1024)}))
            )
            .await
            .is_err()
        );
        assert!(backend.pending.lock().unwrap().is_empty());
        assert!(!backend.is_alive());
        assert!(backend.call::<Value>("following", json!({})).await.is_err());
    }
    #[tokio::test]
    async fn cancelling_a_response_wait_preserves_a_healthy_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("waiting.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (events, _) = mpsc::channel(4);
        let backend = Backend::connect(&path, events).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(server);
        let mut line = String::new();
        let (a, b) = tokio::join!(
            tokio::time::timeout(
                Duration::from_millis(50),
                backend.call::<Value>("wait", json!({}))
            ),
            reader.read_line(&mut line)
        );
        assert!(a.is_err());
        assert!(b.unwrap() > 0);
        assert!(backend.pending.lock().unwrap().is_empty());
        assert!(backend.is_alive());
    }
}
