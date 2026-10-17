use crate::runtime::{Paths, private_write};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocketUpgrade},
    },
    response::Response,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};
use tower_http::services::ServeDir;

#[derive(Clone)]
struct Viewer {
    port: u16,
    token: String,
    closed: tokio::sync::watch::Sender<Option<std::time::Instant>>,
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
struct ViewerConnection(Viewer);
impl Drop for ViewerConnection {
    fn drop(&mut self) {
        if self
            .0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
        {
            self.0.closed.send_replace(Some(std::time::Instant::now()));
        }
    }
}
async fn upgrade(
    State(state): State<Viewer>,
    Path(token): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    use axum::response::IntoResponse;
    if token != state.token {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    ws.protocols(["binary"])
        .on_upgrade(move |socket| async move {
            state
                .active
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let _connection = ViewerConnection(state.clone());
            state.closed.send_replace(None);
            let Ok(tcp) = TcpStream::connect(("127.0.0.1", state.port)).await else {
                return;
            };
            let (mut reader, mut writer) = tcp.into_split();
            let (mut sink, mut stream) = socket.split();
            let to_vnc = async {
                while let Some(Ok(message)) = stream.next().await {
                    match message {
                        Message::Binary(data) => writer.write_all(&data).await?,
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                Ok::<_, std::io::Error>(())
            };
            let from_vnc = async {
                let mut buffer = vec![0u8; 65536];
                loop {
                    let n = reader.read(&mut buffer).await?;
                    if n == 0 {
                        break;
                    }
                    sink.send(Message::Binary(buffer[..n].to_vec().into()))
                        .await
                        .map_err(std::io::Error::other)?;
                }
                Ok::<_, std::io::Error>(())
            };
            tokio::select! {_=to_vnc=>{},_=from_vnc=>{}}
        })
}
struct CredentialFile(std::path::PathBuf);
impl Drop for CredentialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub async fn run(paths: &Paths, no_open: bool) -> Result<()> {
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.runtime.join("viewer.lock"))?;
    lock.try_lock_exclusive()
        .context("已有临时 QQ 查看器正在运行")?;
    let display =
        std::fs::read_to_string(paths.runtime.join("display")).context("QQ 宿主尚未启动")?;
    let display = if display.starts_with(':') {
        display
    } else {
        format!(":{display}")
    };
    let assets = paths.novnc_dir()?;
    let password = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    let password_file = paths.runtime.join("viewer.pass");
    let mut encoder = Command::new("vncpasswd")
        .arg("-f")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    encoder
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{password}\n").as_bytes())
        .await?;
    let output = encoder.wait_with_output().await?;
    if !output.status.success() {
        bail!("生成 VNC 临时凭据失败");
    }
    private_write(&password_file, output.stdout)?;
    let _credentials = CredentialFile(password_file.clone());
    let (port, mut vnc) = {
        let mut ready = None;
        for _ in 0..3 {
            let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))?;
            let port = reservation.local_addr()?.port();
            drop(reservation);
            let mut child = Command::new("x0vncserver")
                .args([
                    "-display",
                    &display,
                    "-localhost",
                    "-AlwaysShared",
                    "-rfbport",
                    &port.to_string(),
                    "-PasswordFile",
                ])
                .arg(&password_file)
                .env("XAUTHORITY", paths.runtime.join("Xauthority"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            for _ in 0..30 {
                if child.try_wait()?.is_some() {
                    break;
                }
                let handshake = tokio::time::timeout(Duration::from_millis(150), async {
                    let mut tcp = TcpStream::connect(("127.0.0.1", port)).await?;
                    let mut banner = [0; 12];
                    tcp.read_exact(&mut banner).await?;
                    Ok::<_, std::io::Error>(banner.starts_with(b"RFB "))
                })
                .await;
                if matches!(handshake, Ok(Ok(true))) {
                    ready = Some((port, child));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if ready.is_some() {
                break;
            }
        }
        ready.context("VNC 未能启动或端口被占用；请查看宿主状态后重试")?
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let http_port = listener.local_addr()?.port();
    let url = format!(
        "http://127.0.0.1:{http_port}/vnc.html?autoconnect=1&resize=scale&path=ws/{token}&password={password}"
    );
    let (closed, mut disconnected) = tokio::sync::watch::channel(None::<std::time::Instant>);
    let router = Router::new()
        .route("/ws/{token}", get(upgrade))
        .fallback_service(ServeDir::new(assets))
        .with_state(Viewer {
            port,
            token,
            closed: closed.clone(),
            active: Default::default(),
        });
    println!("临时 QQ 查看器：{url}\n按 Ctrl+C 关闭；最长保留 15 分钟。");
    if !no_open {
        let _ = crate::platform::open(url.clone()).await;
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
                _ = tokio::time::sleep(Duration::from_secs(900)) => {},
                _ = async {loop {let since=*disconnected.borrow_and_update();match since {Some(since)=>{tokio::select!{_=tokio::time::sleep_until(tokio::time::Instant::from_std(since+Duration::from_secs(10)))=>break,_=disconnected.changed()=>{}}},None=>{if disconnected.changed().await.is_err(){break;}}}}} => {},
            }
        })
        .await?;
    vnc.kill().await?;
    Ok(())
}
