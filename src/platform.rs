use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;
pub type LocalWriteHalf = tokio::io::WriteHalf<LocalStream>;

pub async fn connect(path: &Path) -> Result<LocalStream> {
    #[cfg(unix)]
    {
        Ok(LocalStream::connect(path).await?)
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        for _ in 0..40 {
            match ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(pipe),
                Err(error)
                    if error.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("QQ named pipe is busy")
    }
}
pub fn endpoint(runtime: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        runtime.join("bridge.sock")
    }
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(runtime.to_string_lossy().as_bytes());
        let suffix = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        PathBuf::from(format!(r"\\.\pipe\qqtui-{suffix}"))
    }
}
pub fn private_permissions(path: &Path, directory: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    // Windows uses the ACL inherited from the current user's application-data directory.
    #[cfg(windows)]
    {
        let _ = (path, directory);
    }
    Ok(())
}
pub async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {_=terminate.recv()=>{},result=tokio::signal::ctrl_c()=>result?};
    }
    #[cfg(windows)]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}
fn installed_qq(candidate: PathBuf) -> Option<PathBuf> {
    let candidate = candidate.canonicalize().ok()?;
    (candidate.is_file()
        && candidate
            .parent()?
            .join("resources/app/package.json")
            .is_file())
    .then_some(candidate)
}
#[cfg(target_os = "linux")]
fn qq_package_executable(wrapper: &Path) -> Option<PathBuf> {
    use std::process::Command;
    for manager in ["pacman", "dpkg-query", "rpm"] {
        let mut owner = Command::new(manager);
        match manager {
            "pacman" => {
                owner.args(["-Qoq", "--"]);
            }
            "dpkg-query" => {
                owner.args(["-S"]);
            }
            _ => {
                owner.args(["-qf", "--qf", "%{NAME}"]);
            }
        }
        let output = match owner.arg(wrapper).output() {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };
        let owner = String::from_utf8_lossy(&output.stdout);
        let first = owner.lines().next()?.trim();
        let package = if manager == "dpkg-query" {
            first.split_once(": ")?.0
        } else {
            first
        };
        if package.is_empty() || package.starts_with('-') {
            continue;
        }
        let output = Command::new(manager)
            .arg(match manager {
                "pacman" => "-Qlq",
                "dpkg-query" => "-L",
                _ => "-ql",
            })
            .arg(package)
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let file = PathBuf::from(line);
            if file
                .file_name()
                .is_some_and(|name| name == "qq" || name == "QQ")
                && let Some(path) = installed_qq(file)
            {
                return Some(path);
            }
        }
    }
    None
}
#[cfg(target_os = "linux")]
fn desktop_qq(entry: &freedesktop_desktop_entry::DesktopEntry) -> Option<PathBuf> {
    let name = entry.name::<&str>(&[]).unwrap_or_default();
    if !name.eq_ignore_ascii_case("QQ") && !entry.appid.to_lowercase().ends_with("qq") {
        return None;
    }
    let exec = shell_words::split(entry.exec()?).ok()?;
    for word in exec {
        let path = PathBuf::from(word);
        if !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("qq"))
        {
            continue;
        }
        let candidates = if path.is_absolute() {
            vec![path]
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join(&path))
                .collect()
        };
        for candidate in candidates {
            if let Some(path) = installed_qq(candidate.clone()) {
                return Some(path);
            }
            if candidate.is_file()
                && let Some(path) = qq_package_executable(&candidate)
            {
                return Some(path);
            }
        }
    }
    None
}
pub fn discover_qq() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            for program in if cfg!(windows) {
                ["QQ.exe", "qq.exe"]
            } else {
                ["qq", "QQ"]
            } {
                if let Some(path) = installed_qq(directory.join(program)) {
                    return Some(path);
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        use freedesktop_desktop_entry::{Iter, default_paths};
        for entry in Iter::new(default_paths()).entries::<&str>(None) {
            if let Some(path) = desktop_qq(&entry) {
                return Some(path);
            }
        }
    }
    #[cfg(windows)]
    {
        use winreg::{
            RegKey,
            enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
        };
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            if let Ok(key) = RegKey::predef(hive)
                .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\App Paths\QQ.exe")
                && let Ok(path) = key.get_value::<String, _>("")
                && let Some(path) = installed_qq(PathBuf::from(path))
            {
                return Some(path);
            }
        }
    }
    None
}
pub async fn open(target: String) -> Result<()> {
    tokio::task::spawn_blocking(move || open::that_detached(target)).await??;
    Ok(())
}
pub async fn logs(path: &Path, follow: bool) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("读取 {}", path.display()))?;
    let length = file.metadata().await?.len();
    file.seek(std::io::SeekFrom::Start(length.saturating_sub(512 * 1024)))
        .await?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).await?;
    let text = String::from_utf8_lossy(&data);
    let mut lines = text.lines().rev().take(80).collect::<Vec<_>>();
    lines.reverse();
    for line in lines {
        println!("{line}");
    }
    if follow {
        loop {
            tokio::select! {result=tokio::signal::ctrl_c()=>{result?;break},_=tokio::time::sleep(std::time::Duration::from_millis(250))=>{}}
            if file.metadata().await?.len() < file.stream_position().await? {
                file.rewind().await?;
            }
            data.clear();
            (&mut file).take(64 * 1024).read_to_end(&mut data).await?;
            if !data.is_empty() {
                print!("{}", String::from_utf8_lossy(&data));
                use std::io::Write;
                std::io::stdout().flush()?;
            }
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn desktop_discovery_honors_quoted_registered_paths_without_executing_launchers() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("中文 QQ installation");
        std::fs::create_dir_all(install.join("resources/app")).unwrap();
        let executable = install.join("qq");
        std::fs::write(&executable, []).unwrap();
        std::fs::write(install.join("resources/app/package.json"), "{}").unwrap();
        let entry=freedesktop_desktop_entry::DesktopEntry::from_str::<&str>("qq.desktop",&format!("[Desktop Entry]\nName=QQ\nType=Application\nExec=env GTK_IM_MODULE=fcitx '{}' %U\n",executable.display()),None).unwrap();
        assert_eq!(desktop_qq(&entry), Some(executable.canonicalize().unwrap()));
    }
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn named_pipe_transport_exchanges_bytes_with_a_local_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let temp = tempfile::tempdir().unwrap();
        let name = endpoint(temp.path());
        let mut server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let client = tokio::spawn(async move {
            let mut client = connect(&name).await.unwrap();
            client.write_all(b"request").await.unwrap();
            let mut reply = [0u8; 5];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"reply");
        });
        server.connect().await.unwrap();
        let mut request = [0u8; 7];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"request");
        server.write_all(b"reply").await.unwrap();
        client.await.unwrap();
    }
}
