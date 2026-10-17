use crate::model::*;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
#[derive(Clone)]
pub struct Store {
    jobs: mpsc::Sender<Job>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_owned();
        let (jobs, mut rx) = mpsc::channel::<Job>(128);
        let (ready, initialized) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<Connection> {
                let db = Connection::open(&path)?;
                crate::platform::private_permissions(&path, false)?;
                db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
                    CREATE TABLE IF NOT EXISTS messages(account TEXT, peer TEXT, id TEXT, time INTEGER, seq INTEGER, text TEXT, data TEXT, PRIMARY KEY(account,peer,id));
                    CREATE TABLE IF NOT EXISTS drafts(account TEXT,peer TEXT,data TEXT,PRIMARY KEY(account,peer));
                    CREATE TABLE IF NOT EXISTS snapshots(account TEXT PRIMARY KEY,data TEXT);
                    CREATE TABLE IF NOT EXISTS outbox(account TEXT,id TEXT,data TEXT,draft TEXT,PRIMARY KEY(account,id));
                    CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,data TEXT);")?;
                let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                if version < 1 {
                    db.execute_batch("BEGIN IMMEDIATE; CREATE INDEX IF NOT EXISTS messages_window ON messages(account,peer,time,seq,length(id),id); CREATE INDEX IF NOT EXISTS messages_sequence ON messages(account,peer,json_extract(data,'$.seq')); PRAGMA user_version=1; COMMIT;")?;
                }
                Ok(db)
            })();
            match result {
                Ok(mut db) => {
                    let _ = ready.send(Ok(()));
                    while let Some(job) = rx.blocking_recv() {
                        job(&mut db);
                    }
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            }
        });
        initialized.await??;
        Ok(Self { jobs })
    }
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |db| {
                let _ = tx.send(f(db));
            }))
            .await
            .map_err(|_| anyhow::anyhow!("缓存数据库已关闭"))?;
        rx.await?
    }
    pub async fn save_messages(&self, messages: Vec<Message>) -> Result<()> {
        self.run(move |db| {
            let tx = db.transaction()?;
            for m in messages {
                tx.execute(
                    "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(account,peer,id) DO UPDATE SET time=excluded.time,seq=excluded.seq,text=excluded.text,data=excluded.data WHERE COALESCE(json_extract(messages.data,'$.recalled'),0)=0 OR COALESCE(json_extract(excluded.data,'$.recalled'),0)=1",
                    params![
                        m.account,
                        m.peer.key(),
                        m.id,
                        m.time.parse::<i64>().unwrap_or(0),
                        m.seq.parse::<i64>().unwrap_or(0),
                        m.text,
                        serde_json::to_string(&m)?
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn quoted_message(
        &self,
        account: String,
        peer: Peer,
        id: Option<String>,
        seq: Option<String>,
    ) -> Result<Option<Message>> {
        self.run(move |db| {
            use rusqlite::OptionalExtension;
            for (column, value) in [("id", id), ("json_extract(data,'$.seq')", seq)] {
                if let Some(value) = value.filter(|v| !v.is_empty() && v != "0") {
                    let data: Option<String> = db.query_row(&format!("SELECT data FROM messages WHERE account=?1 AND peer=?2 AND {column}=?3 AND (COALESCE(json_extract(data,'$.system_notice'),0)=0 OR COALESCE(json_extract(data,'$.recalled'),0)=1) AND json_extract(data,'$.reaction_notice') IS NULL LIMIT 1"),params![account,peer.key(),value],|r|r.get(0)).optional()?;
                    if let Some(data) = data { return Ok(Some(serde_json::from_str(&data)?)); }
                }
            }
            Ok(None)
        }).await
    }
    pub async fn messages(
        &self,
        account: String,
        peer: Peer,
        query: Option<String>,
    ) -> Result<Vec<Message>> {
        self.run(move |db| {
            let mut stmt = db.prepare("SELECT data FROM messages WHERE account=?1 AND peer=?2 AND (?3 IS NULL OR instr(lower(text),lower(?3))>0) ORDER BY time DESC,seq DESC,length(id) DESC,id DESC LIMIT 500")?;
            let rows = stmt.query_map(params![account,peer.key(),query], |r| r.get::<_,String>(0))?;
            let mut result = rows.map(|s| Ok(serde_json::from_str(&s?)?)).collect::<Result<Vec<Message>>>()?;
            result.reverse(); Ok(result)
        }).await
    }
    pub async fn cached_window(
        &self,
        account: String,
        peer: Peer,
        anchor: Option<String>,
    ) -> Result<Vec<Message>> {
        if let Some(anchor) = anchor {
            let a = account.clone();
            let p = peer.clone();
            let messages = self.run(move |db| {
                let mut messages = vec![];
                for (comparison, order) in [("<=", "DESC"), (">", "ASC")] {
                    let sql = format!("SELECT data FROM messages WHERE account=?1 AND peer=?2
                        AND (time,seq,length(id),id) {comparison} (SELECT time,seq,length(id),id FROM messages WHERE account=?1 AND peer=?2 AND id=?3)
                        ORDER BY time {order},seq {order},length(id) {order},id {order} LIMIT 250");
                    let mut statement = db.prepare(&sql)?;
                    let rows = statement.query_map(params![a,p.key(),anchor], |r| r.get::<_,String>(0))?;
                    let mut half = rows.map(|row| Ok(serde_json::from_str::<Message>(&row?)?)).collect::<Result<Vec<_>>>()?;
                    if order == "DESC" { half.reverse(); }
                    messages.extend(half);
                }
                Ok(messages)
            }).await?;
            if !messages.is_empty() {
                return Ok(messages);
            }
        }
        self.messages(account, peer, None).await
    }
    pub async fn draft(&self, account: String, peer: Peer) -> Result<Draft> {
        self.run(move |db| {
            let mut q = db.prepare("SELECT data FROM drafts WHERE account=?1 AND peer=?2")?;
            let mut rows = q.query(params![account, peer.key()])?;
            Ok(match rows.next()? {
                Some(r) => serde_json::from_str(&r.get::<_, String>(0)?)?,
                None => Draft::default(),
            })
        })
        .await
    }
    pub fn queue_account_drafts(
        &self,
        account: String,
        drafts: Vec<(Peer, Draft)>,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .try_send(Box::new(move |db| {
                let result = (|| {
                    let transaction = db.transaction()?;
                    for (peer, draft) in drafts {
                        transaction.execute(
                            "INSERT OR REPLACE INTO drafts VALUES(?1,?2,?3)",
                            params![account, peer.key(), serde_json::to_string(&draft)?],
                        )?;
                    }
                    transaction.commit()?;
                    Ok(())
                })();
                let _ = tx.send(result);
            }))
            .map_err(|_| anyhow::anyhow!("保存队列已满，暂不切换本地账号"))?;
        Ok(rx)
    }
    pub fn queue_draft(
        &self,
        account: String,
        peer: Peer,
        draft: Draft,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .try_send(Box::new(move |db| {
                let _ = tx.send(Self::write_draft(db, account, peer, draft));
            }))
            .map_err(|_| anyhow::anyhow!("草稿保存队列暂不可用"))?;
        Ok(rx)
    }
    fn write_draft(db: &mut Connection, account: String, peer: Peer, draft: Draft) -> Result<()> {
        db.execute(
            "INSERT OR REPLACE INTO drafts VALUES(?1,?2,?3)",
            params![account, peer.key(), serde_json::to_string(&draft)?],
        )?;
        Ok(())
    }
    pub async fn save_draft(&self, account: String, peer: Peer, draft: Draft) -> Result<()> {
        self.run(move |db| Self::write_draft(db, account, peer, draft))
            .await
    }
    pub async fn recall(&self, account: String, peer: Peer, seq: String) -> Result<Vec<Message>> {
        self.run(move |db| {
            let tx = db.transaction()?;
            let messages = {
                let mut s = tx
                    .prepare("SELECT data FROM messages WHERE account=?1 AND peer=?2 AND json_extract(data,'$.seq')=?3 AND (COALESCE(json_extract(data,'$.system_notice'),0)=0 OR COALESCE(json_extract(data,'$.recalled'),0)=1) AND json_extract(data,'$.reaction_notice') IS NULL")?;
                let rows = s.query_map(params![account, peer.key(), seq], |r| {
                    r.get::<_, String>(0)
                })?;
                rows.map(|row| Ok(serde_json::from_str::<Message>(&row?)?))
                    .collect::<Result<Vec<_>>>()?
            };
            let mut updated = vec![];
            for mut message in messages {
                message.retract();
                tx.execute(
                    "UPDATE messages SET text=?4,data=?5 WHERE account=?1 AND peer=?2 AND id=?3",
                    params![
                        account,
                        peer.key(),
                        message.id,
                        message.text,
                        serde_json::to_string(&message)?
                    ],
                )?;
                updated.push(message);
            }
            tx.commit()?;
            Ok(updated)
        })
        .await
    }
    pub async fn save_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        self.run(move |db| {
            db.execute(
                "INSERT OR REPLACE INTO snapshots VALUES(?1,?2)",
                params![snapshot.account, serde_json::to_string(&snapshot)?],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn cached_snapshot(&self) -> Result<Option<Snapshot>> {
        self.run(|db| {
            let mut s = db.prepare("SELECT data FROM snapshots ORDER BY rowid DESC LIMIT 1")?;
            let mut rows = s.query([])?;
            Ok(match rows.next()? {
                Some(r) => Some(serde_json::from_str(&r.get::<_, String>(0)?)?),
                None => None,
            })
        })
        .await
    }
    pub async fn stage_send(&self, receipt: Receipt, draft: Draft) -> Result<()> {
        self.run(move |db| {
            let tx = db.transaction()?;
            tx.execute(
                "INSERT INTO outbox VALUES(?1,?2,?3,?4)",
                params![
                    receipt.account,
                    receipt.request_id,
                    serde_json::to_string(&receipt)?,
                    serde_json::to_string(&draft)?
                ],
            )?;
            tx.execute(
                "DELETE FROM drafts WHERE account=?1 AND peer=?2",
                params![receipt.account, receipt.peer.key()],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn receipt(&self, receipt: Receipt) -> Result<()> {
        self.run(move |db| {
            db.execute(
                "UPDATE outbox SET data=?3 WHERE account=?1 AND id=?2 AND (COALESCE(json_extract(data,'$.state'),'')!='confirmed' OR json_extract(?3,'$.state')='confirmed')",
                params![
                    receipt.account,
                    receipt.request_id,
                    serde_json::to_string(&receipt)?
                ],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn outbox(&self, account: String) -> Result<Vec<(Receipt, Draft)>> {
        self.run(move |db| {
            let mut s = db.prepare(
                "SELECT data,draft FROM outbox WHERE account=?1 ORDER BY rowid DESC LIMIT 200",
            )?;
            let rows = s.query_map([account], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.map(|r| {
                let (a, b) = r?;
                Ok((serde_json::from_str(&a)?, serde_json::from_str(&b)?))
            })
            .collect()
        })
        .await
    }
    pub async fn cleanup_media(
        &self,
        cache: std::path::PathBuf,
        mut pins: std::collections::HashSet<String>,
    ) -> Result<()> {
        let pins=self.run(move|db|{
            let mut statement=db.prepare("SELECT data FROM drafts UNION ALL SELECT draft FROM outbox WHERE json_extract(data,'$.state')!='confirmed'")?;
            for row in statement.query_map([],|r|r.get::<_,String>(0))? {let draft:Draft=serde_json::from_str(&row?)?;pins.extend(draft.attachments.into_iter().map(|a|a.path));}
            Ok(pins)
        }).await?;
        tokio::task::spawn_blocking(move || crate::media::prune_cache(&cache, pins)).await??;
        Ok(())
    }
    pub async fn setting(&self, key: String, value: Option<String>) -> Result<Option<String>> {
        self.run(move |db| {
            if let Some(value) = value {
                db.execute(
                    "INSERT OR REPLACE INTO settings VALUES(?1,?2)",
                    params![key, value],
                )?;
                return Ok(None);
            }
            let mut s = db.prepare("SELECT data FROM settings WHERE key=?1")?;
            let mut rows = s.query([key])?;
            Ok(match rows.next()? {
                Some(r) => Some(r.get(0)?),
                None => None,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn peer() -> Peer {
        Peer {
            chat_type: 1,
            uid: "self".into(),
        }
    }
    #[tokio::test]
    async fn sequence_quotes_skip_notices_even_when_the_native_sequence_is_shared() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let original = Message {
            account: "a".into(),
            peer: peer(),
            id: "2".into(),
            seq: "42".into(),
            ..Default::default()
        };
        let notice = Message {
            id: "1".into(),
            reaction_notice: Some("<gtip><url msgseq=\"42\"/></gtip>".into()),
            system_notice: true,
            ..original.clone()
        };
        store.save_messages(vec![notice, original]).await.unwrap();
        assert_eq!(
            store
                .quoted_message("a".into(), peer(), None, Some("42".into()))
                .await
                .unwrap()
                .unwrap()
                .id,
            "2"
        );
        let recalled = store.recall("a".into(), peer(), "42".into()).await.unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].id, "2");
        let messages = store.messages("a".into(), peer(), None).await.unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.id == "1" && m.reaction_notice.is_some())
        );
    }
    #[tokio::test]
    async fn caches_are_scoped_by_account_and_peer_and_search_handles_short_chinese() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let message = Message {
            account: "a".into(),
            peer: peer(),
            id: "90071992547409933".into(),
            time: "2".into(),
            seq: "3".into(),
            text: "中文搜索".into(),
            ..Default::default()
        };
        store.save_messages(vec![message]).await.unwrap();
        assert_eq!(
            store
                .messages("a".into(), peer(), Some("中".into()))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .messages("b".into(), peer(), None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.messages("a".into(), peer(), None).await.unwrap()[0].id,
            "90071992547409933"
        );
    }
    #[tokio::test]
    async fn send_is_durable_before_draft_is_cleared_and_not_staged_twice() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("db");
        let store = Store::open(&file).await.unwrap();
        let draft = Draft {
            text: "important draft".into(),
            ..Default::default()
        };
        store
            .save_draft("a".into(), peer(), draft.clone())
            .await
            .unwrap();
        let receipt = Receipt {
            account: "a".into(),
            peer: peer(),
            request_id: "once".into(),
            state: "submitting".into(),
            ..Default::default()
        };
        store
            .stage_send(receipt.clone(), draft.clone())
            .await
            .unwrap();
        assert!(
            store
                .draft("a".into(), peer())
                .await
                .unwrap()
                .text
                .is_empty()
        );
        assert!(store.stage_send(receipt, draft).await.is_err());
        let reopened = Store::open(&file).await.unwrap();
        let records = reopened.outbox("a".into()).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1.text, "important draft");
    }
    #[tokio::test]
    async fn recall_is_scoped_exact_and_cannot_be_resurrected_by_stale_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let message = Message {
            account: "a".into(),
            peer: peer(),
            id: "1".into(),
            seq: "18446744073709551614".into(),
            text: "retract me".into(),
            ..Default::default()
        };
        let mut other = message.clone();
        other.account = "b".into();
        store
            .save_messages(vec![message.clone(), other])
            .await
            .unwrap();
        assert!(
            store
                .recall("a".into(), peer(), "18446744073709551613".into())
                .await
                .unwrap()
                .is_empty()
        );
        let recalled = store
            .recall("a".into(), peer(), message.seq.clone())
            .await
            .unwrap();
        assert_eq!(recalled.len(), 1);
        assert!(recalled[0].recalled);
        store.save_messages(vec![message]).await.unwrap();
        assert!(store.messages("a".into(), peer(), None).await.unwrap()[0].recalled);
        assert!(!store.messages("b".into(), peer(), None).await.unwrap()[0].recalled);
        assert!(
            store
                .messages("a".into(), peer(), Some("retract me".into()))
                .await
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn cached_window_restores_an_anchor_older_than_the_latest_five_hundred_messages() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let messages = (1..=1200)
            .map(|i| Message {
                account: "a".into(),
                peer: peer(),
                id: i.to_string(),
                time: i.to_string(),
                seq: i.to_string(),
                ..Default::default()
            })
            .collect();
        store.save_messages(messages).await.unwrap();
        let latest = store.messages("a".into(), peer(), None).await.unwrap();
        assert_eq!(latest.len(), 500);
        assert!(!latest.iter().any(|m| m.id == "300"));
        let window = store
            .cached_window("a".into(), peer(), Some("300".into()))
            .await
            .unwrap();
        assert_eq!(window.len(), 500);
        assert_eq!(window.first().unwrap().id, "51");
        assert_eq!(window.last().unwrap().id, "550");
        assert!(window.iter().any(|m| m.id == "300"));
        assert!(
            store
                .cached_window("b".into(), peer(), Some("300".into()))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .cached_window("a".into(), peer(), Some("missing".into()))
                .await
                .unwrap()
                .first()
                .unwrap()
                .id,
            "701"
        );
    }
    #[tokio::test]
    async fn a_late_submission_result_cannot_downgrade_a_durable_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let mut receipt = Receipt {
            account: "a".into(),
            peer: peer(),
            request_id: "once".into(),
            state: "submitting".into(),
            ..Default::default()
        };
        store
            .stage_send(receipt.clone(), Draft::default())
            .await
            .unwrap();
        let late = receipt.clone();
        receipt.state = "confirmed".into();
        receipt.message_id = "90071992547409933".into();
        store.receipt(receipt).await.unwrap();
        store.receipt(late).await.unwrap();
        let rows = store.outbox("a".into()).await.unwrap();
        assert_eq!(rows[0].0.state, "confirmed");
        assert_eq!(rows[0].0.message_id, "90071992547409933");
    }
    #[test]
    #[cfg(unix)]
    fn concurrent_wal_open_close_completes_in_an_isolated_process() {
        use std::process::{Command, Stdio};
        let executable = if cfg!(target_os = "linux") {
            std::path::PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe().unwrap()
        };
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "store::tests::wal_stress_worker",
                "--ignored",
                "--test-threads=1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "WAL stress process failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("SQLite WAL open/close regression exceeded its deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    #[test]
    #[cfg(unix)]
    #[ignore = "invoked by concurrent_wal_open_close_completes_in_an_isolated_process"]
    fn wal_stress_worker() {
        assert!(
            rusqlite::version_number() >= 3_051_002,
            "SQLite 3.51.1 has an upstream Unix VFS deadlock"
        );
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                for _ in 0..60 {
                    let directory=tempfile::tempdir().unwrap();
                    let database=Connection::open(directory.path().join("db")).unwrap();
                    database.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE items(value); INSERT INTO items VALUES(1);").unwrap();
                    assert_eq!(database.query_row("SELECT value FROM items",[],|row|row.get::<_,i64>(0)).unwrap(),1);
                    drop(database);
                }
            });
            }
        });
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    #[tokio::test]
    async fn queued_autosave_precedes_send_staging_and_final_flush() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).await.unwrap();
        let peer = Peer {
            chat_type: 1,
            uid: "p".into(),
        };
        let queued = store
            .queue_draft(
                "a".into(),
                peer.clone(),
                Draft {
                    text: "old".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .stage_send(
                Receipt {
                    account: "a".into(),
                    peer: peer.clone(),
                    request_id: "send".into(),
                    state: SendState::Submitting,
                    ..Default::default()
                },
                Draft {
                    text: "new".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        queued.await.unwrap().unwrap();
        assert!(
            store
                .draft("a".into(), peer.clone())
                .await
                .unwrap()
                .is_empty()
        );
        let queued = store
            .queue_draft(
                "a".into(),
                peer.clone(),
                Draft {
                    text: "older".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .save_draft(
                "a".into(),
                peer.clone(),
                Draft {
                    text: "latest".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        queued.await.unwrap().unwrap();
        assert_eq!(store.draft("a".into(), peer).await.unwrap().text, "latest");
    }
}
