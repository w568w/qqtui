mod accounts;
mod app;
mod backend;
mod media;
mod model;
mod notifications;
mod platform;
mod runtime;
mod store;
mod ui;
mod viewer;
use anyhow::Result;
use clap::{Parser, Subcommand};
use runtime::Paths;

#[derive(Parser)]
#[command(version, about = "QQ 原生服务终端前端")]
struct Cli {
    /// 固定此窗口使用的 QQ 号；default 直接打开原会话
    #[arg(long, global = true)]
    account: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// 初始化用户配置；可移交现有 QQ 会话
    Setup {
        #[arg(long)]
        takeover: bool,
        #[arg(long)]
        qq_path: Option<std::path::PathBuf>,
    },
    /// 查看原生宿主状态
    Status,
    /// 列出可选账号和后台状态
    Accounts,
    /// 检查 QQ 版本、运行组件与原生能力
    Doctor,
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
    #[command(hide = true)]
    Host,
}
#[derive(Subcommand)]
enum BackendCommand {
    Start,
    Stop,
    View {
        #[arg(long)]
        no_open: bool,
    },
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = Paths::discover()?;
    let paths = match cli.account.as_deref() {
        None | Some("default") => base.clone(),
        Some(account) if matches!(cli.command, Some(Command::Host)) => base.for_account(account)?,
        Some(account) => accounts::resolve(&base, account).await?,
    };
    match cli.command {
        None => {
            if cli.account.is_none() {
                if let Some(selected) = accounts::choose(&base).await? {
                    accounts::ensure_started(&base, &selected).await?;
                    accounts::login_if_needed(&selected).await?;
                    app::run(selected).await?;
                }
            } else {
                if paths.account.is_some() {
                    accounts::ensure_started(&base, &paths).await?;
                    accounts::login_if_needed(&paths).await?;
                }
                app::run(paths).await?;
            }
        }
        Some(Command::Accounts) => println!(
            "{}",
            serde_json::to_string_pretty(&accounts::list(&base).await)?
        ),
        Some(Command::Setup { takeover, qq_path }) => {
            accounts::initialize(&base, &paths)?;
            if let Some(path) = qq_path {
                runtime::set_qq_path(&paths, &path)?;
            }
            runtime::setup(&paths, takeover).await?;
        }
        Some(Command::Status) => println!(
            "{}",
            serde_json::to_string_pretty(&runtime::status(&paths).await?)?
        ),
        Some(Command::Doctor) => runtime::doctor(&paths).await?,
        Some(Command::Host) => runtime::host(&paths).await?,
        Some(Command::Backend { command }) => match command {
            BackendCommand::Start => accounts::ensure_started(&base, &paths).await?,
            BackendCommand::Stop => runtime::systemctl(&["stop", &paths.service_name()]).await?,
            BackendCommand::View { no_open } => viewer::run(&paths, no_open).await?,
            BackendCommand::Logs { follow } => {
                platform::logs(&paths.state.join("host.log"), follow).await?;
            }
        },
    }
    Ok(())
}
