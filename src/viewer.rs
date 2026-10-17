#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(not(target_os = "linux"))]
pub async fn run(_paths: &crate::runtime::Paths, _no_open: bool) -> anyhow::Result<()> {
    anyhow::bail!("临时 noVNC 查看器当前仅由 Linux QQ 宿主提供")
}
