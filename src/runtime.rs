use crate::model::HostStatus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{doctor, host, setup, systemctl};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub qq_path: PathBuf,
    pub expected_qq_version: String,
    pub sidebar_width: u16,
    pub sidebar_visible: bool,
    pub show_avatars: bool,
    pub auto_switch_ime: bool,
    pub ime_ascii: Option<String>,
    pub novnc_path: Option<PathBuf>,
    pub keymap: std::collections::BTreeMap<String, String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            qq_path: PathBuf::new(),
            expected_qq_version: String::new(),
            sidebar_width: 26,
            sidebar_visible: true,
            show_avatars: true,
            auto_switch_ime: true,
            ime_ascii: None,
            novnc_path: None,
            keymap: Default::default(),
        }
    }
}
#[derive(Clone)]
pub struct Paths {
    pub account: Option<String>,
    #[cfg(target_os = "linux")]
    pub shared_state: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub runtime: PathBuf,
}
impl Paths {
    pub fn discover() -> Result<Self> {
        let base = directories::BaseDirs::new().context("无法确定用户应用目录")?;
        let state = base
            .state_dir()
            .unwrap_or(base.data_local_dir())
            .join("qqtui");
        let runtime = base
            .runtime_dir()
            .map(|path| path.join("qqtui"))
            .unwrap_or_else(|| state.join("runtime"));
        Ok(Self {
            account: None,
            #[cfg(target_os = "linux")]
            shared_state: state.clone(),
            config: base.config_dir().join("qqtui"),
            state,
            cache: base.cache_dir().join("qqtui"),
            runtime,
        })
    }
    pub fn for_account(&self, account: &str) -> Result<Self> {
        validate_account(account)?;
        let mut paths = self.clone();
        paths.account = Some(account.to_owned());
        for root in [
            &mut paths.config,
            &mut paths.state,
            &mut paths.cache,
            &mut paths.runtime,
        ] {
            *root = root.join("accounts").join(account);
        }
        Ok(paths)
    }
    pub fn known_account(&self) -> Result<Option<String>> {
        let file = self.state.join("active-account.json");
        let bytes = match fs::read(file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let data: serde_json::Value = serde_json::from_slice(&bytes)?;
        let account = data["account"].as_str().context("账号状态缺少 QQ 号")?;
        validate_account(account)?;
        Ok(Some(account.into()))
    }
    pub fn service_name(&self) -> String {
        self.account.as_ref().map_or_else(
            || "qqtui-host.service".into(),
            |account| format!("qqtui-host@{account}.service"),
        )
    }
    pub fn create(&self) -> Result<()> {
        for p in [&self.config, &self.state, &self.cache, &self.runtime] {
            fs::create_dir_all(p)?;
            crate::platform::private_permissions(p, true)?;
            if let Some(account) = &self.account
                && p.file_name().is_some_and(|n| n == account.as_str())
                && let Some(accounts) = p
                    .parent()
                    .filter(|p| p.file_name().is_some_and(|n| n == "accounts"))
            {
                crate::platform::private_permissions(accounts, true)?;
                if let Some(root) = accounts.parent() {
                    crate::platform::private_permissions(root, true)?;
                }
            }
        }
        Ok(())
    }
    pub fn socket(&self) -> PathBuf {
        crate::platform::endpoint(&self.runtime)
    }
    pub fn config(&self) -> Result<Config> {
        let p = self.config.join("config.toml");
        let mut config = if p.exists() {
            toml::from_str::<Config>(&fs::read_to_string(p)?)?
        } else {
            Config::default()
        };
        if config.qq_path.as_os_str().is_empty() {
            config.qq_path = crate::platform::discover_qq().unwrap_or_else(|| "qq".into());
        }
        Ok(config)
    }
    #[cfg(target_os = "linux")]
    pub fn novnc_dir(&self) -> Result<PathBuf> {
        let config = self.config()?;
        if let Some(path) = config.novnc_path {
            anyhow::ensure!(path.join("vnc.html").is_file(), "novnc_path 缺少 vnc.html");
            return Ok(path);
        }
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for root in std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()) {
            for name in ["novnc", "webapps/novnc"] {
                let path = root.join(name);
                if path.join("vnc.html").is_file() {
                    return Ok(path);
                }
            }
        }
        anyhow::bail!("未找到系统 noVNC，请安装 novnc 包，或配置 novnc_path")
    }
    #[cfg(target_os = "linux")]
    pub fn materialize_bridge(&self) -> Result<PathBuf> {
        let files: &[(&str, &[u8])] = &[
            ("bootstrap.cjs", include_bytes!("../bridge/bootstrap.cjs")),
            ("native.cjs", include_bytes!("../bridge/native.cjs")),
            ("journal.cjs", include_bytes!("../bridge/journal.cjs")),
            (
                "presentation.cjs",
                include_bytes!("../bridge/presentation.cjs"),
            ),
            ("accounts.cjs", include_bytes!("../bridge/accounts.cjs")),
            ("desktop.cjs", include_bytes!("../bridge/desktop.cjs")),
            (
                "qq-resources.cjs",
                include_bytes!("../bridge/qq-resources.cjs"),
            ),
            (
                "compatibility.json",
                include_bytes!("../bridge/compatibility.json"),
            ),
            (
                "libqqtui-loader.so",
                include_bytes!(concat!(env!("OUT_DIR"), "/libqqtui-loader.so")),
            ),
        ];
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for (name, bytes) in files {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let dir = self.state.join(format!(
            "bridge-{}",
            hash.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        if dir.exists() {
            for (name, bytes) in files {
                anyhow::ensure!(
                    fs::read(dir.join(name))? == *bytes,
                    "桥接资源校验失败：{name}"
                );
            }
            return Ok(dir);
        }
        let staging = self.state.join(format!(".bridge-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&staging)?;
        crate::platform::private_permissions(&staging, true)?;
        for (name, bytes) in files {
            private_write(&staging.join(name), bytes)?;
        }
        if let Err(error) = fs::rename(&staging, &dir) {
            if !dir.is_dir() {
                return Err(error.into());
            }
            fs::remove_dir_all(staging)?;
        }
        Ok(dir)
    }
}
pub fn validate_account(account: &str) -> Result<()> {
    anyhow::ensure!(
        !account.is_empty()
            && account.len() <= 20
            && !account.starts_with('0')
            && account.bytes().all(|byte| byte.is_ascii_digit())
            && account.parse::<u64>().is_ok(),
        "请输入有效的十进制 QQ 号"
    );
    Ok(())
}
pub fn private_write(path: &Path, data: impl AsRef<[u8]>) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(data.as_ref())?;
    f.sync_all()?;
    Ok(())
}
fn config_lock(paths: &Paths) -> Result<fs::File> {
    use fs2::FileExt;
    fs::create_dir_all(&paths.config)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.config.join("config.lock"))?;
    crate::platform::private_permissions(&paths.config.join("config.lock"), false)?;
    lock.lock_exclusive()?;
    Ok(lock)
}
pub fn set_qq_path(paths: &Paths, path: &Path) -> Result<()> {
    paths.create()?;
    let path = path.canonicalize()?;
    let _lock = config_lock(paths)?;
    let config = paths.config.join("config.toml");
    let mut document = match fs::read_to_string(&config) {
        Ok(text) => text.parse::<toml_edit::DocumentMut>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(error) => return Err(error.into()),
    };
    document["qq_path"] = toml_edit::value(path.to_string_lossy().as_ref());
    let temporary = paths
        .config
        .join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    private_write(&temporary, document.to_string())?;
    fs::rename(temporary, config)?;
    #[cfg(unix)]
    fs::File::open(&paths.config)?.sync_all()?;
    Ok(())
}
pub fn save_ui_config(paths: &Paths, config: &Config) -> Result<()> {
    let _lock = config_lock(paths)?;
    let path = paths.config.join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut document = text.parse::<toml_edit::DocumentMut>()?;
    document["sidebar_width"] = toml_edit::value(i64::from(config.sidebar_width));
    document["sidebar_visible"] = toml_edit::value(config.sidebar_visible);
    let temp = paths
        .config
        .join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    private_write(&temp, document.to_string())?;
    fs::rename(temp, path)?;
    #[cfg(unix)]
    fs::File::open(&paths.config)?.sync_all()?;
    Ok(())
}

pub async fn status(paths: &Paths) -> Result<HostStatus> {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    crate::backend::Backend::connect(&paths.socket(), tx)
        .await?
        .call("host.status", serde_json::json!({}))
        .await
}
#[cfg(not(target_os = "linux"))]
pub async fn setup(paths: &Paths, _takeover: bool) -> Result<()> {
    paths.create()?;
    anyhow::bail!("此平台可连接本地 QQ 桥接；受管理的 QQ 宿主安装当前仅支持 Linux")
}
#[cfg(not(target_os = "linux"))]
pub async fn host(_paths: &Paths) -> Result<()> {
    anyhow::bail!("此平台尚未提供 QQ 宿主加载器")
}
#[cfg(not(target_os = "linux"))]
pub async fn systemctl(_args: &[&str]) -> Result<()> {
    anyhow::bail!("此平台不使用 systemd；请通过本机 QQ 桥接管理宿主")
}
#[cfg(not(target_os = "linux"))]
pub async fn doctor(paths: &Paths) -> Result<()> {
    println!(
        "配置：{}\n桥接：{}",
        paths.config.display(),
        paths.socket().display()
    );
    match status(paths).await {
        Ok(status) => println!("宿主：{}", status.phase),
        Err(error) => println!("宿主：{error}"),
    };
    Ok(())
}

#[cfg(test)]
mod account_tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_novnc_resource_directory_is_validated() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Paths::discover().unwrap();
        paths.config = temp.path().join("config");
        fs::create_dir_all(&paths.config).unwrap();
        let resources = temp.path().join("system novnc");
        fs::create_dir_all(&resources).unwrap();
        let config = Config {
            novnc_path: Some(resources.clone()),
            ..Default::default()
        };
        fs::write(
            paths.config.join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        assert!(paths.novnc_dir().is_err());
        fs::write(resources.join("vnc.html"), "fixture").unwrap();
        assert_eq!(paths.novnc_dir().unwrap(), resources);
    }
    #[test]
    fn account_paths_isolate_runtime_without_copying_the_qq_installation() {
        let base = Paths::discover().unwrap();
        let first = base.for_account("123456").unwrap();
        let second = base.for_account("654321").unwrap();
        assert_ne!(first.socket(), second.socket());
        assert_ne!(first.state, second.state);
        #[cfg(target_os = "linux")]
        assert_eq!(first.shared_state, base.state);
        assert_eq!(first.config.file_name().unwrap(), "123456");
        assert_eq!(first.service_name(), "qqtui-host@123456.service");
        for invalid in [
            "",
            "../123",
            "01",
            "default",
            "1;id",
            "18446744073709551616",
        ] {
            assert!(base.for_account(invalid).is_err(), "{invalid}");
        }
    }
}
