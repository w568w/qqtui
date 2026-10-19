use super::*;
use anyhow::bail;
use serde_json::{Value, json};
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};

pub async fn setup(paths: &Paths, takeover: bool) -> Result<()> {
    paths.create()?;
    let config = paths.config()?;
    verify_qq(&config)?;
    let existing = other_qq(&config)?;
    if !existing.is_empty() && !takeover {
        bail!("发现现有 QQ 会话，请正常退出它，或使用 qqtui setup --takeover 移交宿主");
    }
    if takeover && !existing.is_empty() {
        private_write(
            &paths.state.join("original-launch.json"),
            serde_json::to_vec_pretty(&existing)?,
        )?;
        for (pid, _) in &existing {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        for _ in 0..100 {
            if existing
                .iter()
                .all(|(pid, _)| !Path::new(&format!("/proc/{pid}")).exists())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if existing
            .iter()
            .any(|(pid, _)| Path::new(&format!("/proc/{pid}")).exists())
        {
            bail!("QQ 尚未正常退出，请手动退出后重试；未强制结束会话");
        }
    }
    if !paths.config.join("config.toml").exists() {
        private_write(
            &paths.config.join("config.toml"),
            toml::to_string_pretty(&config)?,
        )?;
    }
    paths.materialize_bridge()?;
    systemctl(&["daemon-reload"]).await?;
    if takeover {
        systemctl(&["start", &paths.service_name()]).await?;
    }
    println!(
        "配置：{}\n启动：qqtui{} backend start",
        paths.config.display(),
        paths
            .account
            .as_ref()
            .map(|account| format!(" --account {account}"))
            .unwrap_or_default()
    );
    Ok(())
}
fn other_qq(config: &Config) -> Result<Vec<(u32, Vec<String>)>> {
    use std::os::unix::fs::MetadataExt;
    let uid = fs::metadata("/proc/self")?.uid();
    let executable = config.qq_path.canonicalize()?;
    let mut processes = vec![];
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let dir = entry.path();
        if fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid)
            || fs::read_link(dir.join("exe")).ok().as_ref() != Some(&executable)
        {
            continue;
        }
        let Ok(cmdline) = fs::read(dir.join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if args.iter().any(|a| a.starts_with("--type=")) {
            continue;
        }
        let managed = fs::read(dir.join("environ"))
            .map(|v| {
                v.split(|b| *b == 0)
                    .any(|s| s.starts_with(b"QQTUI_SOCKET="))
            })
            .unwrap_or(false);
        if !managed {
            processes.push((pid, args));
        }
    }
    Ok(processes)
}
pub fn verify_qq(config: &Config) -> Result<Value> {
    if !config.qq_path.is_absolute() {
        bail!("未找到 QQ 原生可执行文件，请运行 qqtui setup --qq-path <QQ可执行文件>");
    }
    let package = config
        .qq_path
        .parent()
        .context("Invalid QQ executable path")?
        .join("resources/app/package.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&package).with_context(|| format!("读取 {}", package.display()))?,
    )?;
    if !config.expected_qq_version.is_empty() && manifest["version"] != config.expected_qq_version {
        bail!(
            "QQ 版本 {} 尚未验证，当前支持 {}",
            manifest["version"],
            config.expected_qq_version
        );
    }
    let profiles: Value = serde_json::from_str(include_str!("../../bridge/compatibility.json"))?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let version = manifest["version"].as_str().context("QQ version missing")?;
    if profiles
        .get(std::env::consts::OS)
        .and_then(|p| p.get(arch))
        .and_then(|p| p.get(version))
        .is_none()
    {
        bail!("QQ {version} 在当前平台尚未验证，需添加兼容记录");
    }
    Ok(manifest)
}
pub async fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .await?;
    if !status.success() {
        bail!("systemctl 操作失败：{}", args.join(" "));
    }
    Ok(())
}
pub async fn doctor(paths: &Paths) -> Result<()> {
    let config = paths.config()?;
    let manifest = verify_qq(&config)?;
    println!("QQ {}：{}", manifest["version"], config.qq_path.display());
    for name in [
        "Xvfb",
        "xauth",
        "x0vncserver",
        "vncpasswd",
        "xkbcomp",
        "dbus-daemon",
        "wl-copy",
        "wl-paste",
        "xclip",
    ] {
        let program = PathBuf::from(name);
        let found = if program.is_absolute() {
            program.is_file()
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|d| d.join(&program).is_file())
        };
        println!("{name}：{}", if found { "可用" } else { "缺失" });
    }
    println!(
        "剪贴板会话：{}",
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "Wayland"
        } else if std::env::var_os("DISPLAY").is_some() {
            "X11"
        } else {
            "无图形剪贴板会话"
        }
    );
    println!(
        "noVNC：{}",
        if paths.novnc_dir().is_ok() {
            "可用"
        } else {
            "缺失；安装 novnc 包或配置 novnc_path"
        }
    );
    match status(paths).await {
        Ok(s) => {
            println!("宿主：{}；原生能力：{}", s.phase, s.capabilities.join(", "));
            for error in s.errors {
                println!("诊断：{error}");
            }
        }
        Err(e) => println!("宿主：{e}"),
    }
    println!(
        "配置：{}\n缓存：{}",
        paths.config.display(),
        paths.state.display()
    );
    Ok(())
}
struct DisplayOwner {
    socket: PathBuf,
    lock: PathBuf,
    pid: u32,
    device: u64,
    inode: u64,
}
impl DisplayOwner {
    fn capture(display: &str, pid: u32) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let number = display.trim_start_matches(':');
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        anyhow::ensure!(
            fs::read_to_string(&lock)?.trim().parse::<u32>()? == pid,
            "独立显示编号已被其他进程占用"
        );
        let metadata = fs::symlink_metadata(&socket)?;
        Ok(Self {
            socket,
            lock,
            pid,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    fn cleanup(&self) {
        use std::os::unix::fs::MetadataExt;
        if Path::new(&format!("/proc/{}", self.pid)).exists() {
            return;
        }
        let owned_lock = || {
            fs::read_to_string(&self.lock)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                == Some(self.pid)
        };
        if !owned_lock() {
            return;
        }
        if fs::symlink_metadata(&self.socket)
            .is_ok_and(|m| m.dev() == self.device && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.socket);
        }
        if owned_lock() {
            let _ = fs::remove_file(&self.lock);
        }
    }
}
struct Children(Vec<(&'static str, Child)>);
impl Children {
    fn get(&mut self, name: &str) -> &mut Child {
        &mut self
            .0
            .iter_mut()
            .find(|(n, _)| *n == name)
            .expect("registered host component")
            .1
    }
}
impl Drop for Children {
    fn drop(&mut self) {
        for (_, c) in &mut self.0 {
            let _ = c.start_kill();
        }
    }
}
pub async fn host(paths: &Paths) -> Result<()> {
    use fs2::FileExt;
    paths.create()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.runtime.join("host.lock"))?;
    lock.try_lock_exclusive().context("QQ 宿主已经运行")?;
    if UnixStream::connect(paths.socket()).await.is_ok() {
        bail!("已有桥接服务正在使用此 socket");
    }
    if paths.socket().exists() {
        fs::remove_file(paths.socket())?;
    }
    let config = paths.config()?;
    if !other_qq(&config)?.is_empty() {
        bail!("现有桌面 QQ 仍在运行，请正常退出或使用 setup --takeover");
    }
    let mut package = verify_qq(&config)?;
    let app = config.qq_path.parent().unwrap().join("resources/app");
    let original_main = app.join(package["main"].as_str().context("QQ main field missing")?);
    let bridge = paths.materialize_bridge()?;
    // ld.so splits LD_PRELOAD on spaces and colons, even within quoted paths.
    // Keep the supervisor's descriptor alive so arbitrary XDG paths work.
    use std::os::fd::AsRawFd;
    let loader_file = fs::File::open(bridge.join("libqqtui-loader.so"))?;
    let preload = format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        loader_file.as_raw_fd()
    );
    // Electron joins main with its package directory even when main is absolute.
    let relative = relative_path(&app, &bridge.join("bootstrap.cjs"));
    package["main"] = json!(relative);
    let manifest = paths.runtime.join("package.json");
    private_write(&manifest, serde_json::to_vec(&package)?)?;
    let display_guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.shared_state.join("display-allocation.lock"))?;
    display_guard.lock_exclusive()?;
    let display = (90..200)
        .find(|n| {
            !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
                && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
        })
        .context("没有可用的 X11 display")?;
    let display = format!(":{display}");
    let authority = paths.runtime.join("Xauthority");
    private_write(&authority, [])?;
    let cookie = uuid::Uuid::new_v4().simple().to_string();
    let auth_status = Command::new("xauth")
        .arg("-f")
        .arg(&authority)
        .args(["add", &display, ".", &cookie])
        .status()
        .await?;
    if !auth_status.success() {
        bail!("生成 X11 认证失败");
    }
    let mut children = Children(vec![]);
    let mut owned_display = None;
    let startup: Result<Option<(&'static str, std::process::ExitStatus)>> = async {
        let mut xvfb = Command::new("Xvfb");
        xvfb.args([
            &display,
            "-screen",
            "0",
            "1280x900x24",
            "-nolisten",
            "tcp",
            "-auth",
        ])
        .arg(&authority);
        children.0.push(("Xvfb", xvfb.kill_on_drop(true).spawn()?));
        for _ in 0..50 {
            if Path::new(&format!("/tmp/.X11-unix/X{}", &display[1..])).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !Path::new(&format!("/tmp/.X11-unix/X{}", &display[1..])).exists()
            || children.get("Xvfb").try_wait()?.is_some()
        {
            bail!("QQ 独立显示环境启动失败");
        }
        owned_display=Some(DisplayOwner::capture(&display,children.get("Xvfb").id().context("Xvfb 已退出")?)?);
        private_write(&paths.runtime.join("display"), &display)?;
        drop(display_guard);
        let session_runtime = paths.runtime.join("qq-session");
        fs::create_dir_all(&session_runtime)?;
        fs::set_permissions(&session_runtime, fs::Permissions::from_mode(0o700))?;
        let bus_config = session_runtime.join("bus.conf");
        use std::os::unix::fs::MetadataExt;
        let uid = fs::metadata("/proc/self")?.uid();
        private_write(
            &bus_config,
            format!(
                "<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow user=\"{uid}\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>"
            ),
        )?;
        let mut bus = Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--config-file={}", bus_config.display()))
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut address = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address),
        )
        .await??;
        if !address.starts_with("unix:") {
            bail!("QQ 私有 D-Bus 启动失败");
        }
        children.0.push(("D-Bus", bus));
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(paths.state.join("host.log"))?;
        let lease_dir = paths.shared_state.join("account-leases");
        clean_account_leases(&lease_dir)?;
        let mut qq = Command::new(&config.qq_path);
        qq.env("QQTUI_LEASE_DIR", lease_dir);
        if let Some(account) = &paths.account {
            qq.env("QQTUI_ACCOUNT", account)
                .env("QQTUI_PROFILE_DIR", paths.state.join("qq-profile"));
        }
        children.0.push((
            "QQ",
            qq.arg("--ozone-platform=x11")
                .env("DISPLAY", &display)
                .env("XAUTHORITY", &authority)
                .env("QT_QPA_PLATFORM", "xcb")
                .env("GDK_BACKEND", "x11")
                .env("XDG_RUNTIME_DIR", &session_runtime)
                .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
                .env_remove("DBUS_STARTER_ADDRESS")
                .env_remove("DBUS_STARTER_BUS_TYPE")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("XDG_ACTIVATION_TOKEN")
                .env_remove("DESKTOP_STARTUP_ID")
                .env("LD_PRELOAD", &preload)
                .env("QQTUI_PACKAGE", app.join("package.json"))
                .env("QQTUI_MANIFEST", manifest)
                .env("QQTUI_ORIGINAL_MAIN", original_main)
                .env("QQTUI_SOCKET", paths.socket())
                .env("QQTUI_STATE_DIR", &paths.state)
                .env("QQTUI_CACHE_DIR", &paths.cache)
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .kill_on_drop(true)
                .spawn()?,
        ));
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let exit = 'supervise: loop {
            tokio::select! {
                _=terminate.recv()=>break None,
                _=tokio::signal::ctrl_c()=>break None,
                _=tokio::time::sleep(Duration::from_millis(100))=>{
                    for (name,child) in &mut children.0 {if let Some(status)=child.try_wait()?{break 'supervise Some((*name,status));}}
                }
            }
        };
        Ok(exit)
    }.await;
    // Ask QQ to quit first, then bound shutdown. Xvfb outlives QQ cleanup.
    if let Some((_, qq)) = children.0.iter_mut().find(|(name, _)| *name == "QQ")
        && let Some(pid) = qq.id()
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        let _ = tokio::time::timeout(Duration::from_secs(10), qq.wait()).await;
    }
    for (_, child) in children.0.iter_mut().rev() {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            if tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
    drop(children);
    if let Some(display) = owned_display {
        display.cleanup();
    }
    if paths.socket().exists() {
        fs::remove_file(paths.socket())?;
    }
    let exit = startup?;
    if let Some((name, status)) = exit
        && (name != "QQ" || !status.success())
    {
        bail!("QQ 宿主组件 {name} 退出（{status}），详见 host.log");
    }
    Ok(())
}
fn clean_account_leases(dir: &Path) -> Result<()> {
    use fs2::FileExt;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("cleanup.lock"))?;
    guard.lock_exclusive()?;
    for entry in fs::read_dir(dir)? {
        let file = entry?.path();
        if file.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(&file) else { continue };
        let Ok(owner) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(pid) = owner["pid"].as_u64()
            && pid > 0
            && (!Path::new(&format!("/proc/{pid}")).exists()
                || owner["start"].as_str().is_some_and(|start| {
                    fs::read_to_string(format!("/proc/{pid}/stat"))
                        .ok()
                        .and_then(|s| {
                            s.rsplit_once(") ").map(|(_, tail)| {
                                tail.split_whitespace().nth(19).unwrap_or("").to_owned()
                            })
                        })
                        .as_deref()
                        != Some(start)
                }))
        {
            fs::remove_file(file)?;
        }
    }
    Ok(())
}
fn relative_path(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let same = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let mut result = PathBuf::new();
    for _ in same..a.len() {
        result.push("..");
    }
    for c in &b[same..] {
        result.push(c);
    }
    result.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_settings_preserve_comments_and_other_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Paths::discover().unwrap();
        paths.config = temp.path().into();
        let file = paths.config.join("config.toml");
        fs::write(&file,"# preserved\nqq_path = '/custom/QQ'\n[unrelated]\nvalue = 42\n[keymap]\nsend = 'ctrl-s'\n").unwrap();
        let config = Config {
            sidebar_width: 32,
            sidebar_visible: false,
            ..Default::default()
        };
        save_ui_config(&paths, &config).unwrap();
        let saved = fs::read_to_string(file).unwrap();
        assert!(saved.contains("# preserved"));
        assert!(saved.contains("value = 42"));
        assert!(saved.contains("send = 'ctrl-s'"));
        let decoded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(decoded.qq_path, PathBuf::from("/custom/QQ"));
        assert_eq!(decoded.sidebar_width, 32);
        assert!(!decoded.sidebar_visible);
        set_qq_path(&paths, temp.path()).unwrap();
        let saved = fs::read_to_string(paths.config.join("config.toml")).unwrap();
        let decoded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(decoded.qq_path, temp.path().canonicalize().unwrap());
        assert_eq!(decoded.sidebar_width, 32);
        assert!(!decoded.sidebar_visible);
        assert!(saved.contains("# preserved"));
        assert!(saved.contains("value = 42"));
    }
    #[test]
    fn updating_the_bridge_preserves_the_loader_already_opened_by_a_running_host() {
        use std::io::Read;
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Paths::discover().unwrap();
        paths.state = temp.path().into();
        let directory = paths.materialize_bridge().unwrap();
        let loader = directory.join("libqqtui-loader.so");
        let mut running_loader = fs::File::open(&loader).unwrap();
        assert_eq!(paths.materialize_bridge().unwrap(), directory);
        let mut bytes = vec![];
        running_loader.read_to_end(&mut bytes).unwrap();
        assert!(bytes.starts_with(b"\x7fELF"));
        fs::write(loader, b"corrupt").unwrap();
        assert!(paths.materialize_bridge().is_err());
    }
}
