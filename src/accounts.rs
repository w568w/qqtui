use crate::{
    backend::Backend,
    model::HostStatus,
    runtime::{self, Paths},
};
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::{
    layout::{Constraint, Flex, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde::Serialize;
use std::time::Duration;

#[derive(Serialize)]
pub struct Account {
    pub account: Option<String>,
    pub name: String,
    pub uin: Option<String>,
    pub phase: String,
}
async fn probe(paths: &Paths) -> Option<HostStatus> {
    tokio::time::timeout(Duration::from_secs(1), runtime::status(paths))
        .await
        .ok()?
        .ok()
}
pub async fn resolve(base: &Paths, account: &str) -> Result<Paths> {
    runtime::validate_account(account)?;
    if let Some(current) = probe(base).await
        && current.account == account
    {
        return Ok(base.clone());
    }
    if base.known_account()?.as_deref() == Some(account) {
        return Ok(base.clone());
    }
    base.for_account(account)
}
pub async fn list(base: &Paths) -> Vec<Account> {
    let current = probe(base).await;
    let current_id = current
        .as_ref()
        .map(|s| s.account.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| base.known_account().ok().flatten());
    let mut accounts = vec![Account {
        account: None,
        uin: current_id.clone(),
        name: current_id.clone().unwrap_or_else(|| "未登录账号".into()),
        phase: current
            .as_ref()
            .map_or_else(|| "未启动".into(), |s| s.phase.clone()),
    }];
    if let Some(status) = &current {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let known = async {
            let backend = Backend::connect(&base.socket(), tx).await?;
            backend
                .call::<serde_json::Value>(
                    "accounts.list",
                    serde_json::json!({"account":status.account}),
                )
                .await
        };
        if let Ok(Ok(result)) = tokio::time::timeout(Duration::from_secs(1), known).await
            && let Some(items) = result["items"].as_array()
        {
            for item in items {
                let Some(id) = item["account"]
                    .as_str()
                    .filter(|id| runtime::validate_account(id).is_ok())
                else {
                    continue;
                };
                let name = item["name"].as_str().unwrap_or("").trim();
                let name = if name.is_empty() {
                    id.into()
                } else {
                    format!("{id} · {name}")
                };
                if current_id.as_deref() == Some(id) {
                    accounts[0].name = name;
                    continue;
                }
                accounts.push(Account {
                    account: Some(id.into()),
                    uin: Some(id.into()),
                    name,
                    phase: "未启动".into(),
                });
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(base.config.join("accounts")) {
        let mut ids: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|id| runtime::validate_account(id).is_ok())
            .collect();
        ids.sort();
        for id in ids {
            if current_id.as_ref() == Some(&id)
                || accounts.iter().any(|a| a.account.as_ref() == Some(&id))
            {
                continue;
            }
            accounts.push(Account {
                account: Some(id.clone()),
                uin: Some(id.clone()),
                name: id,
                phase: "未启动".into(),
            });
        }
    }
    for account in &mut accounts[1..] {
        if let Ok(paths) = base.for_account(account.account.as_deref().unwrap())
            && let Some(status) = probe(&paths).await
        {
            account.phase = status.phase;
        }
    }
    accounts
}
struct Terminal;
impl Drop for Terminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub async fn choose(base: &Paths) -> Result<Option<Paths>> {
    let accounts = list(base).await;
    pick(base, accounts, false).await
}

pub async fn choose_backend(base: &Paths) -> Result<Option<Paths>> {
    let accounts = list(base).await;
    if accounts.len() == 1 {
        return Ok(Some(match &accounts[0].account {
            Some(account) => base.for_account(account)?,
            None => base.clone(),
        }));
    }
    pick(base, accounts, true).await
}

async fn pick(base: &Paths, accounts: Vec<Account>, backend: bool) -> Result<Option<Paths>> {
    let mut terminal = ratatui::init();
    let _restore = Terminal;
    let mut events = EventStream::new();
    let mut state = ListState::default().with_selected(Some(0));
    let mut adding = false;
    let mut input = String::new();
    let mut error = String::new();
    loop {
        terminal.draw(|frame| {
            let vertical = Layout::vertical([Constraint::Length(14)]).flex(Flex::Center).split(frame.area())[0];
            let area = Layout::horizontal([Constraint::Length(64)]).flex(Flex::Center).split(vertical)[0];
            let block = Block::default().borders(Borders::ALL).title(if backend { " 选择 QQ 宿主 " } else { " 选择 QQ 账号 · 每个窗口一个账号 " });
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let sections = Layout::vertical([Constraint::Min(1),Constraint::Length(3)]).split(inner);
            if adding {
                frame.render_widget(Paragraph::new(format!("\n QQ 号：{input}\n\n {error}\n\n 沿用原 QQ 聊天数据；登录验证通过 QQ 查看器完成。")),sections[0]);
            } else {
                let rows = accounts.iter().map(|account|ListItem::new(format!(" {}   [{}]",account.name,account.phase)))
                    .chain((!backend).then_some(ListItem::new(" ＋ 添加账号")));
                frame.render_stateful_widget(List::new(rows).highlight_style(Style::default().fg(Color::Cyan)).highlight_symbol("› "),sections[0],&mut state);
            }
            frame.render_widget(Paragraph::new(vec![Line::from(if adding {" Enter 确认 · Esc 返回"}else if backend {" j/k 或 ↑/↓ 选择 · Enter 确认 · q 退出"}else{" j/k 或 ↑/↓ 选择 · Enter 打开 · a 添加 · q 退出"}),Line::from(" 关闭窗口后 QQ 仍保持在线。")]),sections[1]);
        })?;
        let Some(event) = events.next().await else {
            return Ok(None);
        };
        let Event::Key(key) = event? else { continue };
        if key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return Ok(None);
        }
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if adding {
            match key.code {
                KeyCode::Esc => {
                    adding = false;
                    input.clear();
                    error.clear();
                }
                KeyCode::Enter => match runtime::validate_account(&input) {
                    Ok(()) => {
                        let paths = if accounts[0].uin.as_deref() == Some(input.as_str()) {
                            base.clone()
                        } else {
                            base.for_account(&input)?
                        };
                        return Ok(Some(paths));
                    }
                    Err(e) => error = e.to_string(),
                },
                KeyCode::Char(ch) if ch.is_ascii_digit() && input.len() < 20 => {
                    input.push(ch);
                    error.clear();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                _ => {}
            }
        } else {
            let index = state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('a') if !backend => adding = true,
                KeyCode::Down | KeyCode::Char('j') => {
                    state.select(Some((index + 1).min(accounts.len() - usize::from(backend))))
                }
                KeyCode::Up | KeyCode::Char('k') => state.select(Some(index.saturating_sub(1))),
                KeyCode::Enter if index == accounts.len() && !backend => adding = true,
                KeyCode::Enter => {
                    return Ok(Some(match &accounts[index].account {
                        Some(account) => base.for_account(account)?,
                        None => base.clone(),
                    }));
                }
                _ => {}
            }
        }
    }
}

pub fn initialize(base: &Paths, paths: &Paths) -> Result<()> {
    if paths.account.is_some() && !paths.config.join("config.toml").exists() {
        paths.create()?;
        runtime::private_write(
            &paths.config.join("config.toml"),
            toml::to_string_pretty(&base.config()?)?,
        )?;
    }
    Ok(())
}
pub async fn ensure_started(base: &Paths, paths: &Paths) -> Result<()> {
    if let Some(status) = probe(paths).await {
        if status.phase == "ready"
            && let Some(account) = &paths.account
        {
            anyhow::ensure!(&status.account == account, "宿主登录的账号与窗口不匹配");
        }
        return Ok(());
    }
    if let Some(account) = &paths.account {
        if let Some(current) = probe(base).await
            && current.account == *account
        {
            anyhow::bail!("此账号已在运行，请在选择框打开对应账号");
        }
        initialize(base, paths)?;
    }
    runtime::setup(paths, false).await?;
    runtime::systemctl(&["start", &paths.service_name()]).await?;
    Ok(())
}

pub async fn login_if_needed(paths: &Paths) -> Result<()> {
    let mut login_since = None;
    let mut quick_login = false;
    let mut login_wait = Duration::from_secs(2);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut last_phase = String::new();
    while std::time::Instant::now() < deadline {
        if let Some(status) = probe(paths).await {
            if status.phase != last_phase {
                eprintln!("QQ 宿主：{}", status.phase);
                last_phase = status.phase.clone();
            }
            if status.phase == "ready" {
                return Ok(());
            }
            if status.phase == "incompatible" {
                anyhow::bail!("{}", status.errors.join("；"));
            }
            if status.phase == "login" {
                if !quick_login {
                    let (tx, _rx) = tokio::sync::mpsc::channel(4);
                    if let Ok(backend) = Backend::connect(&paths.socket(), tx).await {
                        let account = paths.account.clone().or(paths.known_account()?);
                        match backend
                            .call::<serde_json::Value>(
                                "host.login",
                                serde_json::json!({"account":account}),
                            )
                            .await
                        {
                            Ok(result) if result["attempted"] == true => {
                                quick_login = true;
                                login_wait = Duration::from_secs(15);
                                login_since = Some(std::time::Instant::now());
                                eprintln!("QQ 宿主：正在自动登录");
                            }
                            Ok(result) if result["pending"] == true => login_since = None,
                            Ok(_) => {}
                            Err(error) => {
                                quick_login = true;
                                eprintln!("自动登录失败：{error}");
                            }
                        }
                    }
                }
                let since = login_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= login_wait {
                    println!("在 QQ 页面完成登录后，在此终端按 Ctrl+C 关闭临时查看器并进入聊天。");
                    crate::viewer::run(paths, false).await?;
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("QQ 宿主尚未就绪，请查看对应账号的 backend logs");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn explicit_original_account_resolves_to_its_existing_slot_while_offline() {
        let temp = tempfile::tempdir().unwrap();
        let mut base = Paths::discover().unwrap();
        base.state = temp.path().join("state");
        base.runtime = temp.path().join("runtime");
        std::fs::create_dir_all(&base.state).unwrap();
        std::fs::write(
            base.state.join("active-account.json"),
            "{\"account\":\"123456\"}",
        )
        .unwrap();
        assert_eq!(
            resolve(&base, "123456").await.unwrap().socket(),
            base.socket()
        );
        assert_ne!(
            resolve(&base, "654321").await.unwrap().socket(),
            base.socket()
        );
        assert_eq!(list(&base).await[0].uin.as_deref(), Some("123456"));
    }
}
