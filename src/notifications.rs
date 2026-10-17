use crate::{
    model::{Conversation, safe_text},
    runtime::Paths,
};
use anyhow::Result;
use std::{collections::HashMap, fs::OpenOptions};
use tokio::sync::mpsc;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Notice {
    pub key: String,
    pub title: String,
    pub body: String,
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
        tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                let mut pending: HashMap<_, _> =
                    batch.into_iter().map(|n| (n.key.clone(), n)).collect();
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                while let Ok(batch) = rx.try_recv() {
                    for n in batch {
                        pending.insert(n.key.clone(), n);
                    }
                }
                for notice in pending.into_values() {
                    let result = tokio::task::spawn_blocking(move || {
                        let body = if cfg!(all(unix, not(target_os = "macos"))) {
                            markup(&notice.body)
                        } else {
                            safe_text(&notice.body).chars().take(512).collect()
                        };
                        notify_rust::Notification::new()
                            .appname("qqtui")
                            .summary(&safe_text(&notice.title))
                            .body(&body)
                            .show()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    })
                    .await;
                    if let Err(error) = result.unwrap_or_else(|e| Err(e.to_string())) {
                        let _ = error_tx.try_send(format!("桌面通知失败：{error}"));
                    }
                }
            }
        });
        Ok(Some(Self {
            tx,
            errors,
            _owner: owner,
        }))
    }
    pub fn submit(&self, items: Vec<Notice>) -> Result<(), Vec<Notice>> {
        self.tx.try_send(items).map_err(|e| e.into_inner())
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
