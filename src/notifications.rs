use crate::{
    model::{Conversation, Peer, safe_text},
    runtime::Paths,
};
use anyhow::Result;
use std::{collections::HashMap, fs::OpenOptions};
use tokio::sync::mpsc;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Target {
    pub account: String,
    pub peer: Peer,
    pub id: String,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Notice {
    pub key: String,
    pub title: String,
    pub body: String,
    pub target: Option<Target>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Batch {
    pub items: Vec<Notice>,
}
pub fn unread(items: &[Conversation]) -> u64 {
    items.iter().fold(0u64, |total, c| {
        total.saturating_add(if c.children.is_empty() {
            c.unread.parse().unwrap_or(0)
        } else {
            unread(&c.children)
        })
    })
}
pub fn title(account: &str, items: &[Conversation], chat: &str, total: Option<u64>) -> String {
    format!(
        "[{}] qqtui · {} · {}",
        total.unwrap_or_else(|| unread(items)),
        safe_text(account),
        safe_text(chat)
    )
}
fn markup(text: &str) -> String {
    safe_text(text)
        .chars()
        .take(512)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub struct Notifier {
    tx: mpsc::Sender<Vec<Notice>>,
    pub errors: mpsc::Receiver<String>,
    pub activated: mpsc::Receiver<Target>,
    worker: tokio::task::JoinHandle<()>,
    _owner: std::fs::File,
}
impl Notifier {
    pub fn start(paths: &Paths) -> Result<Option<Self>> {
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.runtime.join("notifications.lock"))?;
        match fs2::FileExt::try_lock_exclusive(&owner) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        let (tx, mut rx) = mpsc::channel::<Vec<Notice>>(8);
        let (error_tx, errors) = mpsc::channel(1);
        let (activation_tx, activated) = mpsc::channel(32);
        let worker = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            let mut handles = std::collections::VecDeque::<tokio::task::AbortHandle>::new();
            let waiters = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
            loop {
                let batch = tokio::select! {
                    batch = rx.recv() => match batch { Some(batch) => batch, None => break },
                    result = tasks.join_next(), if !tasks.is_empty() => {
                        if let Some(Ok(Err(error))) = result {
                            let _ = error_tx.try_send(format!("桌面通知失败：{error}"));
                        }
                        continue;
                    }
                };
                let mut pending: HashMap<_, _> =
                    batch.into_iter().map(|n| (n.key.clone(), n)).collect();
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                while let Ok(batch) = rx.try_recv() {
                    for n in batch {
                        pending.insert(n.key.clone(), n);
                    }
                }
                for notice in pending.into_values() {
                    handles.retain(|h| !h.is_finished());
                    if handles.len() >= 64 {
                        handles.pop_front().unwrap().abort();
                    }
                    let activation_tx = activation_tx.clone();
                    let waiters = waiters.clone();
                    handles.push_back(tasks.spawn(show(notice, activation_tx, waiters)));
                }
            }
        });
        Ok(Some(Self {
            tx,
            errors,
            activated,
            worker,
            _owner: owner,
        }))
    }
    pub fn submit(&self, items: Vec<Notice>) -> Result<(), Vec<Notice>> {
        self.tx.try_send(items).map_err(|e| e.into_inner())
    }
}
impl Drop for Notifier {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
async fn show(
    notice: Notice,
    activated: mpsc::Sender<Target>,
    waiters: std::sync::Arc<tokio::sync::Semaphore>,
) -> Result<(), String> {
    let mut notification = notify_rust::Notification::new();
    let body = if cfg!(all(unix, not(target_os = "macos"))) {
        markup(&notice.body)
    } else {
        safe_text(&notice.body).chars().take(512).collect()
    };
    notification
        .appname("qqtui")
        .summary(&safe_text(&notice.title))
        .body(&body);
    if notice.target.is_some() {
        notification.action("default", "查看消息");
    }
    let respond = move |response: &notify_rust::NotificationResponse| {
        if (response.is_default_action()
            || matches!(response, notify_rust::NotificationResponse::Action(key) if key == "default"))
            && let Some(target) = notice.target
        {
            let _ = activated.try_send(target);
        }
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = waiters;
        let handle = notification.show_async().await.map_err(|e| e.to_string())?;
        handle.wait_for_action_async(respond).await;
        Ok(())
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Native callbacks block until activation/dismissal. Keep them outside the
        // Tokio blocking pool so closing a TUI never waits for desktop interaction.
        let permit = waiters.try_acquire_owned().map_err(|e| e.to_string())?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("qqtui-notification".into())
            .spawn(move || {
                let _permit = permit;
                let result = notification
                    .show()
                    .map_err(|e| e.to_string())
                    .and_then(|handle| {
                        handle.wait_for_response(respond).map_err(|e| e.to_string())
                    });
                let _ = tx.send(result);
            })
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_counts_use_leaf_messages_and_markup_is_inert() {
        let items = vec![
            Conversation {
                unread: "2".into(),
                children: vec![
                    Conversation {
                        unread: "14".into(),
                        ..Default::default()
                    },
                    Conversation {
                        unread: "3".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            Conversation {
                unread: "5".into(),
                ..Default::default()
            },
        ];
        assert_eq!(unread(&items), 22);
        assert!(title("123", &items, "chat", None).starts_with("[22]"));
        assert!(title("123", &items, "chat", Some(3)).starts_with("[3]"));
        assert_eq!(markup("<b>&\x1b"), "&lt;b&gt;&amp;");
    }
    #[tokio::test]
    async fn one_frontend_per_account_owns_desktop_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = Paths::discover().unwrap();
        paths.runtime = temp.path().into();
        let first = Notifier::start(&paths).unwrap().unwrap();
        assert!(Notifier::start(&paths).unwrap().is_none());
        drop(first);
        assert!(Notifier::start(&paths).unwrap().is_some());
    }
}
