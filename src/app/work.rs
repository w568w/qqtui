use super::controller::Controller;
use super::*;
#[derive(Clone)]
pub struct RequestContext {
    pub(crate) account: String,
    pub(crate) view: uuid::Uuid,
    pub(crate) native: Option<(uuid::Uuid, String)>,
}
pub enum Work {
    GroupFiles {
        token: uuid::Uuid,
        peer: Peer,
        folders: Vec<GroupFile>,
        result: Result<GroupFiles>,
    },
    UnreadTarget {
        peer: Peer,
        token: uuid::Uuid,
        result: Result<Option<String>>,
    },
    AccountSaved {
        token: uuid::Uuid,
        result: Result<HostStatus>,
    },
    Avatar {
        uid: String,
        token: uuid::Uuid,
        result: Result<Avatar>,
    },
    Permission {
        peer: Peer,
        generation: u64,
        result: Result<SpeechPermission>,
    },
    Scoped {
        scope: RequestContext,
        work: Box<Work>,
    },
    DraftSaved {
        peer: Peer,
        version: u64,
        result: Result<()>,
    },
    EmojiCatalog {
        token: uuid::Uuid,
        purpose: EmojiPurpose,
        result: Result<Vec<Sticker>>,
    },
    StickerPreview {
        token: uuid::Uuid,
        id: String,
        result: Result<String>,
    },
    StickerChosen {
        token: uuid::Uuid,
        peer: Peer,
        result: Result<Attachment>,
    },
    Reactions {
        token: uuid::Uuid,
        result: Result<Message>,
    },
    ReactionMembers {
        token: uuid::Uuid,
        message: Message,
        reaction: Reaction,
        result: Result<ReactionPage>,
    },
    ReactionRefresh {
        peer: Peer,
        seq: String,
        complete: bool,
        result: Result<HistoryPage>,
    },
    Forward {
        token: uuid::Uuid,
        root: Message,
        path: Vec<String>,
        result: Result<HistoryPage>,
    },
    Quote {
        peer: Peer,
        key: String,
        token: uuid::Uuid,
        result: Result<Option<Message>>,
    },
    Connection {
        id: uuid::Uuid,
        epoch: Option<String>,
        work: Box<Work>,
    },
    Connected(Result<Backend>),
    Status(Result<HostStatus>),
    Snapshot(Result<Snapshot>),
    Cached {
        peer: Peer,
        version: u64,
        messages: Result<Vec<Message>>,
        draft: Result<Draft>,
    },
    History {
        peer: Peer,
        generation: u64,
        position: HistoryPoint,
        result: Result<HistoryPage>,
    },
    Staged {
        peer: Peer,
        receipt: Receipt,
        draft: Draft,
        epoch: String,
        proceed: oneshot::Sender<()>,
    },
    RestoreDraft {
        peer: Peer,
        version: u64,
        draft: Draft,
        state: String,
        existing: Result<Draft>,
    },
    StageFailed {
        epoch: String,
        message: String,
    },
    Profile {
        token: uuid::Uuid,
        peer: Peer,
        result: Result<Profile>,
    },
    Recalled {
        peer: Peer,
        messages: Vec<Message>,
    },
    ConversationRead {
        result: Result<()>,
    },
    Read {
        peer: Peer,
        token: uuid::Uuid,
        last_id: String,
        result: Result<()>,
    },
    RecallSubmitted {
        peer: Peer,
        id: String,
    },
    Receipt(Result<Receipt>),
    Clipboard {
        peer: Peer,
        generation: u64,
        result: Result<Clipboard>,
    },
    Attachment {
        peer: Peer,
        generation: u64,
        result: Result<Attachment>,
    },
    Members {
        token: uuid::Uuid,
        peer: Peer,
        mention: bool,
        result: Result<Vec<Member>>,
    },
    Contacts {
        token: uuid::Uuid,
        result: Result<Vec<Conversation>>,
    },
    Search {
        token: uuid::Uuid,
        peer: Peer,
        query: String,
        result: Result<Vec<Message>>,
    },
    Resolved {
        account: String,
        peer: Peer,
        id: String,
        element: String,
        result: Result<ResolvedMedia>,
        open: bool,
        forward_path: Vec<String>,
    },
    Outbox(Result<Vec<(Receipt, Draft)>>),
    Error(String),
    Refresh,
    Folder {
        token: uuid::Uuid,
        peer: Peer,
        result: Result<Vec<Conversation>>,
    },
}
#[derive(Clone)]
pub(crate) struct WorkSender {
    pub(crate) sender: mpsc::Sender<Work>,
    pub(crate) scope: RequestContext,
}
impl WorkSender {
    pub(crate) async fn send(&self, work: Work) -> Result<(), mpsc::error::SendError<()>> {
        self.sender
            .send(Work::Scoped {
                scope: self.scope.clone(),
                work: Box::new(work),
            })
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }
    pub(crate) async fn send_local(&self, work: Work) -> Result<(), mpsc::error::SendError<()>> {
        let mut scope = self.scope.clone();
        scope.native = None;
        self.sender
            .send(Work::Scoped {
                scope,
                work: Box::new(work),
            })
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }
    pub(crate) async fn receipt(
        &self,
        result: Result<Receipt>,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.send(Work::Receipt(result)).await
    }
}
impl Work {
    pub(crate) fn staging_completion(&self) -> bool {
        match self {
            Work::Staged { .. }
            | Work::StageFailed { .. }
            | Work::DraftSaved { .. }
            | Work::AccountSaved { .. } => true,
            Work::Scoped { work, .. } | Work::Connection { work, .. } => work.staging_completion(),
            _ => false,
        }
    }
}
pub(crate) fn apply_receipt(app: &mut App, store: &Store, tx: WorkSender, receipt: Receipt) {
    if receipt.account != app.account {
        return;
    }
    let newly_failed = receipt.state == "failed"
        && app
            .receipts
            .get(&receipt.request_id)
            .is_none_or(|(old, _)| old.state != "failed");
    if newly_failed {
        app.failure_times
            .insert(receipt.request_id.clone(), Instant::now());
    }
    if let Some((old, _)) = app.receipts.get_mut(&receipt.request_id) {
        if old.state == "confirmed" && receipt.state != "confirmed" {
            return;
        }
        *old = receipt.clone();
    }
    if receipt.state == SendState::Confirmed
        && let Some((_, draft)) = app.receipts.get_mut(&receipt.request_id)
    {
        for a in &mut draft.attachments {
            a.lease = None;
        }
    }
    if receipt.state == SendState::Failed && !newly_failed {
        return;
    }
    app.notice = match receipt.state.as_str() {
        "confirmed" => "发送已确认",
        "submitted" => "已提交，等待 QQ 确认",
        "failed" => "发送失败",
        _ => "发送结果未知，重连后核对",
    }
    .into();
    if newly_failed {
        app.notice_deadline = Some((app.notice.clone(), Instant::now() + Duration::from_secs(5)));
    }
    let store = store.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = store.receipt(receipt).await {
            let _ = tx.send(Work::Error(e.to_string())).await;
        }
    });
}
pub(crate) fn apply_work(app: &mut App, controller: &mut Controller, store: &Store, work: Work) {
    app.dirty = true;
    match work {
        Work::UnreadTarget {
            peer,
            token,
            result,
        } => {
            let Some(chat) = app.chats.get_mut(&peer) else {
                return;
            };
            let jump = match chat.unread {
                UnreadState::Loading {
                    token: expected,
                    jump,
                } if expected == token => jump,
                _ => return,
            };
            chat.unread = match result {
                Ok(id) => UnreadState::Ready(id),
                Err(error) => UnreadState::Failed(error.to_string()),
            };
            if jump && app.selected_peer.as_ref() == Some(&peer) {
                controller.action(app, Action::FirstUnread);
            }
        }

        Work::AccountSaved { token, result } => {
            if app.pending_account != Some(token) {
                return;
            }
            app.pending_account = None;
            app.staging = false;
            match result {
                Ok(status) => {
                    if status.phase == "ready" && status.protocol_version == 2 {
                        app.change_account(status.account.clone());
                    }
                    apply_work(app, controller, store, Work::Status(Ok(status)));
                }
                Err(error) => {
                    app.ready = false;
                    app.notice = format!("旧账号草稿保存或账号确认失败，保持本地视图：{error}");
                }
            }
        }

        Work::Avatar { uid, token, result } => {
            if app.avatar_pending.get(&uid) != Some(&token) {
                return;
            }
            app.avatar_pending.remove(&uid);
            let avatars = std::rc::Rc::make_mut(&mut app.avatars);
            if avatars.len() >= 256
                && let Some(id) = avatars.keys().next().cloned()
            {
                avatars.remove(&id);
            }
            if let Ok(value) = &result
                && app
                    .avatars
                    .get(&uid)
                    .and_then(|v| v.as_ref().ok())
                    .is_some_and(|old| old.version != value.version)
            {
                app.media_invalidations.push(value.path.clone());
            }
            std::rc::Rc::make_mut(&mut app.avatars).insert(uid, result.map_err(|e| e.to_string()));
        }
        Work::Permission {
            peer,
            generation,
            result,
        } => {
            if let Some(c) = app.chats.get_mut(&peer) {
                c.permission_pending = false;
                if c.permission_generation != generation {
                    c.permission_at = None;
                    return;
                }
                c.permission_at = Some(Instant::now());
                c.permission = Some(result.unwrap_or_else(|e| SpeechPermission {
                    state: "unknown".into(),
                    scope: "unknown".into(),
                    reason: format!("无法确认群权限：{e}，切换会话可重试"),
                    until: None,
                    can_moderate: false,
                }));
            }
        }
        Work::Scoped { scope, work } => {
            if scope.account != app.account || scope.view != app.context {
                return;
            }
            if let Some((id, epoch)) = scope.native
                && (app.epoch != epoch || controller.backend.as_ref().is_none_or(|b| b.id() != id))
            {
                return;
            }
            apply_work(app, controller, store, *work);
        }
        Work::DraftSaved {
            peer,
            version,
            result,
        } => {
            let success = result.is_ok();
            if let Some(chat) = app.chats.get_mut(&peer)
                && chat.draft_saving == Some(version)
            {
                chat.draft_saving = None;
                match result {
                    Ok(()) if version == chat.draft_version => chat.dirty_since = None,
                    Err(error) => {
                        chat.dirty_since = Some(Instant::now() + Duration::from_secs(1));
                        app.notice = format!("保存草稿失败，将重试：{error}");
                    }
                    _ => {}
                }
            }
            if app.queued_send.as_ref().is_some_and(|(p, _)| p == &peer) {
                let queued = app.queued_send.take().unwrap();
                app.staging = false;
                if success
                    && app.selected_peer.as_ref() == Some(&peer)
                    && app.chat().is_some_and(|c| c.draft_version == queued.1)
                {
                    controller.action(app, Action::Send);
                }
            }
        }
        Work::EmojiCatalog {
            token,
            purpose,
            result,
        } => {
            if app.complete_dialog(token) {
                match result {
                    Ok(mut items) => {
                        if matches!(purpose, EmojiPurpose::React(_)) {
                            items.retain(|item| item.kind == "face");
                            for (symbol, name) in [
                                ('👍', "点赞"),
                                ('❤', "爱心"),
                                ('😂', "笑哭"),
                                ('🎉', "庆祝"),
                            ] {
                                items.push(Sticker {
                                    id: format!("unicode:{}", symbol as u32),
                                    kind: "unicode".into(),
                                    name: format!("{symbol} {name}"),
                                    path: String::new(),
                                    emoji_id: (symbol as u32).to_string(),
                                    emoji_type: "2".into(),
                                });
                            }
                        }
                        app.modal = Modal::EmojiPicker {
                            token,
                            purpose,
                            items,
                            query: String::new(),
                            index: 0,
                        };
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::StickerPreview { token, id, result } => {
            app.sticker_previews.remove(&(token, id.clone()));
            if let Modal::EmojiPicker {
                token: active,
                items,
                ..
            } = &mut app.modal
                && token == *active
                && let Some(sticker) = items.iter_mut().find(|s| s.id == id)
            {
                match result {
                    Ok(path) => sticker.path = path,
                    Err(e) => {
                        app.operation_error(&e);
                        app.sticker_errors.insert((token, id), e.to_string());
                    }
                }
            }
        }
        Work::StickerChosen {
            token,
            peer,
            result,
        } => {
            if let Some(chat) = app.chats.get_mut(&peer) {
                chat.composer_pending = chat.composer_pending.saturating_sub(1);
            }
            if app.complete_dialog(token) {
                app.modal = Modal::None;
                match result {
                    Ok(attachment) => {
                        if let Some(chat) = app.chats.get_mut(&peer) {
                            chat.draft.attachments.push(attachment);
                            chat.changed();
                        }
                        app.mode = Mode::Insert;
                        app.focus = Focus::Messages;
                    }
                    Err(e) => app.operation_error(&e),
                }
            }
        }
        Work::Reactions { token, result } => {
            if app.complete_dialog(token) {
                match result {
                    Ok(message) => {
                        if let Some(chat) = app.chats.get_mut(&message.peer) {
                            chat.receive(vec![message.clone()]);
                        }
                        let updated = message.clone();
                        let store = store.clone();
                        tokio::spawn(async move {
                            let _ = store.save_messages(vec![updated]).await;
                        });
                        app.modal = Modal::Reactions { message, index: 0 };
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::ReactionMembers {
            token,
            message,
            reaction,
            result,
        } => {
            if app.complete_dialog(token) {
                match result {
                    Ok(page) => {
                        app.modal = Modal::ReactionMembers {
                            message,
                            reaction,
                            page,
                            index: 0,
                        }
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::ReactionRefresh {
            peer,
            seq,
            complete,
            result,
        } => {
            if complete {
                app.reaction_refreshes.remove(&(peer.clone(), seq));
            }
            match result {
                Ok(page) => {
                    if let Some(chat) = app.chats.get_mut(&peer) {
                        for update in &page.messages {
                            if let Some(message) = chat
                                .messages
                                .iter_mut()
                                .find(|m| m.id == update.id && !m.recalled)
                            {
                                message.reactions = update.reactions.clone();
                                message.touch();
                            }
                        }
                    }
                    if let Modal::Reactions { message, .. } = &mut app.modal
                        && let Some(update) = page
                            .messages
                            .iter()
                            .find(|m| m.peer == message.peer && m.id == message.id)
                    {
                        message.reactions = update.reactions.clone();
                    }
                    let store = store.clone();
                    tokio::spawn(async move {
                        let _ = store.save_messages(page.messages).await;
                    });
                }
                Err(e) if complete => app.operation_error(&e),
                Err(_) => {}
            }
        }

        Work::Quote {
            peer,
            key,
            token,
            result,
        } => {
            let chat = app.chats.entry(peer.clone()).or_default();
            if !matches!(chat.quotes.get(&key),Some(QuoteState::Loading(current)) if *current==token)
            {
                return;
            }
            let original = match result {
                Ok(message) => message,
                Err(error) => {
                    if chat.quote_jump.as_ref().is_some_and(|(k, _)| k == &key) {
                        app.notice = format!("读取引用失败：{error}");
                    }
                    None
                }
            };
            let target = original.as_ref().map(|m| m.id.clone());
            chat.quotes.insert(
                key.clone(),
                original
                    .map(|m| QuoteState::Ready(Box::new(m)))
                    .unwrap_or(QuoteState::Missing),
            );
            if chat.quote_jump.as_ref().is_some_and(|(k, _)| k == &key) {
                let (_, source) = chat.quote_jump.take().unwrap();
                if Some(&peer) == app.selected_peer.as_ref()
                    && chat.selected.as_ref() == Some(&source)
                {
                    if let Some(id) = target {
                        controller.jump(app, peer, id);
                    } else {
                        app.notice = "引用原消息不可用".into();
                    }
                }
            }
            let queued = app.chat().and_then(|chat| {
                let (key, source) = chat.quote_jump.as_ref()?;
                if chat.selected.as_ref() != Some(source)
                    || matches!(chat.quotes.get(key), Some(QuoteState::Loading(_)))
                {
                    return None;
                }
                chat.messages.iter().find(|m| &m.id == source).cloned()
            });
            if let Some(source) = queued {
                controller.quote(app, source, true);
            }
        }
        Work::Connection { id, epoch, work } => {
            if controller
                .backend
                .as_ref()
                .is_some_and(|backend| backend.id() == id)
                && epoch.as_ref().is_none_or(|epoch| epoch == &app.epoch)
            {
                apply_work(app, controller, store, *work);
            }
        }
        Work::Recalled { peer, messages } => {
            let chat = app.chats.entry(peer).or_default();
            let messages = messages
                .into_iter()
                .filter(|m| !chat.loaded || chat.messages.iter().any(|current| current.id == m.id))
                .collect();
            chat.merge(messages);
            app.notice = "消息已撤回".into();
        }
        Work::ConversationRead { result } => {
            app.sync_needed = true;
            app.notice = match result {
                Ok(()) => "已请求标记已读，正在同步原生状态".into(),
                Err(e) => format!("标记已读失败：{e}"),
            };
        }
        Work::Read {
            peer,
            token,
            last_id,
            result,
        } => {
            if let Some(chat) = app.chats.get_mut(&peer)
                && chat.read.pending == Some(token)
            {
                chat.read.pending = None;
                match result {
                    Ok(()) => {
                        chat.read.confirmed = last_id;
                        chat.read.retry_after = None;
                    }
                    Err(error) => {
                        chat.read.retry_after = Some(Instant::now() + Duration::from_secs(2));
                        app.notice = format!("标记已读失败，将在可见时重试：{error}");
                    }
                }
            }
        }
        Work::RecallSubmitted { peer, id } => {
            app.notice = if app
                .chats
                .get(&peer)
                .and_then(|c| c.messages.iter().find(|m| m.id == id))
                .is_some_and(|m| m.recalled)
            {
                "消息已撤回"
            } else {
                "撤回请求已提交，等待原生更新"
            }
            .into();
        }

        Work::Profile {
            token,
            peer,
            result,
        } => {
            if Some(&peer) == app.selected_peer.as_ref() && app.complete_dialog(token) {
                match result {
                    Ok(profile) => app.modal = Modal::Profile(profile),
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::Folder {
            token,
            peer,
            result,
        } => {
            if Some(&peer) == app.selected_peer.as_ref() && app.complete_dialog(token) {
                match result {
                    Ok(children) => {
                        if let Some(parent) = app.conversations.iter_mut().find(|c| c.peer == peer)
                        {
                            parent.children = children.clone();
                        }
                        if app.selected_peer.as_ref() == Some(&peer) {
                            app.modal = Modal::Contacts {
                                query: String::new(),
                                index: 0,
                                items: children,
                            };
                        }
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }

        Work::Refresh => app.sync_needed = true,
        Work::Connected(result) => {
            controller.connecting = false;
            match result {
                Ok(backend) => {
                    if !backend.is_alive() {
                        controller.backend = None;
                        controller.status_pending = false;
                        app.connected = false;
                        app.ready = false;
                        app.notice = "QQ 连接在初始化前已关闭，正在重试".into();
                        return;
                    }
                    app.connected = true;
                    app.notice = "已连接宿主，正在同步…".into();
                    controller.backend = Some(backend.clone());
                    controller.status_pending = true;
                    let tx = controller.tx.clone();
                    tokio::spawn(async move {
                        let _ = tx
                            .send(Work::Connection {
                                id: backend.id(),
                                epoch: None,
                                work: Box::new(Work::Status(
                                    backend.call("events.subscribe", json!({})).await,
                                )),
                            })
                            .await;
                    });
                }
                Err(e) => app.operation_error(&e),
            }
        }
        Work::Status(result) => {
            controller.status_pending = false;
            let again = std::mem::take(&mut controller.status_again);
            match result {
                Ok(s) => {
                    if s.phase == "ready" && s.protocol_version != 2 {
                        app.ready = false;
                        app.notice =
                            "桥接协议版本不匹配，请使用当前发行包重新 setup 并重启对应宿主".into();
                        return;
                    }
                    if s.phase == "ready"
                        && ["history_navigation", "request_context"]
                            .iter()
                            .any(|required| !s.capabilities.iter().any(|cap| cap == required))
                    {
                        app.ready = false;
                        app.notice =
                            "宿主版本缺少历史定位能力或请求上下文能力，请运行 qqtui backend stop 后重新 start"
                                .into();
                        return;
                    }
                    let session_changed = app.account != s.account || app.epoch != s.epoch;
                    let changed = !app.ready || session_changed;
                    if s.phase == "ready" && app.account != s.account {
                        if app.pending_account.is_some() {
                            return;
                        }
                        let drafts = app
                            .chats
                            .iter()
                            .filter(|(_, c)| c.draft_loaded && c.dirty_since.is_some())
                            .map(|(peer, c)| (peer.clone(), c.draft.clone()))
                            .collect::<Vec<_>>();
                        if drafts.is_empty() {
                            app.change_account(s.account.clone());
                        } else {
                            app.ready = false;
                            let queued = match store
                                .queue_account_drafts(app.account.clone(), drafts)
                            {
                                Ok(done) => done,
                                Err(error) => {
                                    app.notice = format!("旧账号草稿未保存，保持本地视图：{error}");
                                    return;
                                }
                            };
                            let Some(backend) = controller.backend.clone() else {
                                return;
                            };
                            let tx = controller.sender(app);
                            let token = uuid::Uuid::new_v4();
                            app.pending_account = Some(token);
                            app.queued_send = None;
                            app.staging = true;
                            app.notice = "正在保存旧账号草稿".into();
                            tokio::spawn(async move {
                                let result = async {
                                    queued.await??;
                                    backend.call("host.status", json!({})).await
                                }
                                .await;
                                let _ = tx.send_local(Work::AccountSaved { token, result }).await;
                            });
                            return;
                        }
                    }
                    if changed && s.phase == "ready" {
                        if session_changed {
                            app.staging = false;
                        }
                        app.syncing = false;
                        app.close_lifecycle_modals();
                        app.epoch = s.epoch.clone();
                        app.seq = s.seq;
                        app.reset_native_pending();
                        for chat in app.chats.values_mut() {
                            chat.loaded = false;
                            chat.loading = false;
                            chat.pending_history = None;
                            chat.exhausted = false;
                            chat.generation += 1;
                            chat.read = ReadState::default();
                            chat.permission = None;
                            chat.permission_pending = false;
                            chat.permission_at = None;
                        }
                    }
                    app.capabilities = s.capabilities.iter().cloned().collect();
                    app.ready = s.phase == "ready";
                    if app.ready && changed {
                        app.sync_needed = true;
                        app.sync_urgent = true;
                    }
                    if !app.ready {
                        app.notice = if s.errors.is_empty() {
                            format!("QQ 等待登录：{}", app.login_command())
                        } else {
                            s.errors.join("；")
                        };
                    }
                }
                Err(e) => {
                    app.notice = e.to_string();
                    app.ready = false;
                }
            }
            if again {
                controller.status();
            }
        }
        Work::Snapshot(result) => match result {
            Ok(snapshot) => {
                if snapshot.account != app.account || snapshot.epoch != app.epoch {
                    app.syncing = false;
                    app.sync_needed = true;
                    controller.status();
                    return;
                }
                let lagged = app.epoch == snapshot.epoch && app.seq > snapshot.seq;
                let account_changed = app.account != snapshot.account;
                let first = app.epoch != snapshot.epoch || app.chat().is_none_or(|c| !c.loaded);
                app.apply_snapshot(snapshot.clone());
                app.sync_needed |= lagged;
                if first || account_changed {
                    app.notice = "原生会话状态已同步".into();
                }
                let store = store.clone();
                let tx = controller.sender(app);
                let account = app.account.clone();
                tokio::spawn(async move {
                    if let Err(e) = store.save_snapshot(snapshot).await {
                        let _ = tx.send(Work::Error(e.to_string())).await;
                    }
                    if account_changed || first {
                        let result = store.outbox(account.clone()).await;
                        let _ = tx.send_local(Work::Outbox(result)).await;
                    }
                });
                if (first || account_changed)
                    && let Some(peer) = app.selected_peer.clone()
                {
                    controller.load_chat(app, &peer);
                }
            }
            Err(e) => {
                app.syncing = false;
                app.notice = e.to_string();
            }
        },
        Work::Cached {
            peer,
            version,
            messages,
            draft,
        } => {
            let mut failed = None;
            let chat = app.chats.entry(peer).or_default();
            match messages {
                Ok(mut messages) => {
                    if chat.loaded {
                        messages.retain(|m| {
                            m.recalled && chat.messages.iter().any(|current| current.id == m.id)
                        });
                    }
                    chat.merge(messages);
                }
                Err(e) => failed = Some(e),
            }
            match draft {
                Ok(draft) if chat.draft_version == version && chat.draft_version == 0 => {
                    chat.draft = draft;
                    chat.draft_loaded = true;
                    chat.draft.cursor = chat.draft.cursor.min(chat.draft.text.len());
                    while !chat.draft.text.is_char_boundary(chat.draft.cursor) {
                        chat.draft.cursor -= 1;
                    }
                }
                Err(e) => failed = Some(e),
                _ => {}
            }
            if let Some(error) = failed {
                app.operation_error(error);
            }
        }
        Work::Forward {
            token,
            root,
            path,
            result,
        } => {
            if app.complete_dialog(token) {
                match result {
                    Ok(page) => {
                        app.modal = Modal::Forward {
                            root,
                            path,
                            messages: page.messages,
                            index: 0,
                            scroll: 0,
                        }
                    }
                    Err(error) => {
                        app.modal = Modal::None;
                        app.notice = error.to_string();
                    }
                }
            }
        }
        Work::History {
            peer,
            generation,
            position,
            result,
        } => {
            let chat = app.chats.entry(peer.clone()).or_default();
            if chat.generation != generation {
                return;
            }
            chat.loading = false;
            if let Some(position) = chat.pending_history.take() {
                controller.history(app, peer.clone(), position);
                return;
            }
            match result {
                Ok(page) => {
                    if matches!(position, HistoryPoint::Latest | HistoryPoint::Around(_))
                        && page.anchor_found
                        && !chat.viewport.follow
                        && let Some(anchor) =
                            chat.viewport.anchor.as_ref().or(chat.selected.as_ref())
                        && !page.messages.iter().any(|message| &message.id == anchor)
                    {
                        let position = HistoryPoint::Around(anchor.clone());
                        controller.history(app, peer.clone(), position);
                        return;
                    }
                    match &position {
                        HistoryPoint::Latest | HistoryPoint::Around(_) => {
                            chat.exhausted = !page.has_before;
                            chat.has_later = page.has_after;
                            chat.replace_history(page.messages);
                            if matches!(position, HistoryPoint::Latest) {
                                chat.new_messages = 0;
                            }
                            if !page.anchor_found {
                                chat.viewport.anchor = None;
                                app.notice = "原阅读位置已不在 QQ 记录中，已加载最新历史".into();
                            }
                        }
                        HistoryPoint::Before(_) => {
                            chat.exhausted = !page.has_before;
                            chat.merge(page.messages);
                        }
                        HistoryPoint::After(_) => {
                            chat.has_later = page.has_after;
                            chat.merge(page.messages);
                        }
                    }
                    chat.loaded = true;
                }
                Err(e) => app.operation_error(&e),
            }
        }
        Work::RestoreDraft {
            peer,
            version,
            draft,
            state,
            existing,
        } => match existing {
            Ok(existing) => {
                let chat = app.chats.entry(peer.clone()).or_default();
                if chat.draft_version != version {
                    app.notice = "目标草稿已修改，未覆盖".into();
                } else {
                    if !chat.draft_loaded {
                        chat.draft = existing;
                        chat.draft_loaded = true;
                    }
                    if chat.draft.is_empty() {
                        chat.draft = draft;
                        chat.changed();
                        app.notice = format!("已恢复草稿；原发送状态：{state}");
                        if app.selected_peer.as_ref() == Some(&peer) {
                            app.mode = Mode::Insert;
                            app.focus = Focus::Messages;
                        }
                    } else {
                        app.notice = "目标会话已有草稿，未覆盖".into();
                    }
                }
            }
            Err(error) => app.notice = format!("读取草稿失败，未恢复：{error}"),
        },
        Work::Staged {
            peer,
            receipt,
            draft,
            epoch,
            proceed,
        } => {
            if epoch == app.epoch {
                if let Some(c) = app.chats.get_mut(&peer) {
                    app.receipts
                        .insert(receipt.request_id.clone(), (receipt, draft));
                    c.draft = Draft::default();
                    c.changed();
                    let _ = proceed.send(());
                } else {
                    app.notice = "发送记录已保存，可从发送记录恢复草稿".into();
                }
                app.staging = false;
            }
        }
        Work::Receipt(result) => match result {
            Ok(receipt) => apply_receipt(app, store, controller.sender(app), receipt),
            Err(e) => app.operation_error(&e),
        },
        Work::Clipboard {
            peer,
            generation,
            result,
        } => {
            let chat = app.chats.entry(peer.clone()).or_default();
            chat.composer_pending = chat.composer_pending.saturating_sub(1);
            if generation != chat.composer_generation {
                return;
            }
            match result {
                Ok(clipboard) => {
                    let c = app.chats.entry(peer).or_default();
                    match clipboard {
                        Clipboard::Text(text) => {
                            let pos = c.draft.cursor;
                            c.edit(pos, pos, &text);
                        }
                        Clipboard::Attachments(files) => {
                            c.draft.attachments.extend(files);
                            c.changed();
                        }
                    }
                }
                Err(e) => app.operation_error(&e),
            }
        }
        Work::Attachment {
            peer,
            generation,
            result,
        } => {
            let chat = app.chats.entry(peer.clone()).or_default();
            chat.composer_pending = chat.composer_pending.saturating_sub(1);
            if generation != chat.composer_generation {
                return;
            }
            match result {
                Ok(attachment) => {
                    let c = app.chats.entry(peer).or_default();
                    c.draft.attachments.push(attachment);
                    c.changed();
                }
                Err(e) => app.operation_error(&e),
            }
        }
        Work::GroupFiles {
            token,
            peer,
            folders,
            result,
        } => {
            if Some(&peer) == app.selected_peer.as_ref() && app.complete_dialog(token) {
                match result {
                    Ok(page) => {
                        app.modal = Modal::GroupFiles {
                            peer,
                            folders,
                            page,
                            index: 0,
                        }
                    }
                    Err(error) => {
                        app.modal = Modal::None;
                        app.operation_error(&error);
                    }
                }
            }
        }
        Work::Members {
            token,
            peer,
            mention,
            result,
        } => {
            if Some(&peer) == app.selected_peer.as_ref() && app.complete_dialog(token) {
                match result {
                    Ok(members) => {
                        app.modal = Modal::Members {
                            query: String::new(),
                            index: 0,
                            members,
                            mention,
                        }
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::Contacts { token, result } => {
            if app.complete_dialog(token) {
                match result {
                    Ok(items) => {
                        app.modal = Modal::Contacts {
                            query: String::new(),
                            index: 0,
                            items,
                        }
                    }
                    Err(e) => {
                        app.modal = Modal::None;
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::Search {
            token,
            peer,
            query,
            result,
        } => {
            if Some(&peer) == app.selected_peer.as_ref() && app.complete_dialog(token) {
                match result {
                    Ok(results) => {
                        if let Modal::Search {
                            query: current,
                            results: old,
                            index,
                        } = &mut app.modal
                            && *current == query
                        {
                            *old = results;
                            *index = 0;
                        }
                    }
                    Err(e) => app.operation_error(&e),
                }
            }
        }
        Work::Resolved {
            account,
            peer,
            id,
            element,
            forward_path,
            result,
            open,
        } => {
            let key = Message {
                account: account.clone(),
                peer: peer.clone(),
                id: id.clone(),
                forward_path: forward_path.clone(),
                ..Default::default()
            }
            .media_key(&element);
            let open = app.pending_images.remove(&key).unwrap_or(false) || open;
            if let Err(error) = &result {
                if app.failed_images.len() >= 128 {
                    app.failed_images.clear();
                }
                app.failed_images.insert(key.clone(), error.to_string());
            }
            match result {
                Ok(resource) => {
                    app.failed_images.remove(&key);
                    let path = resource.path.clone();
                    let warning = resource.warning.clone();
                    if let Modal::Preview {
                        message,
                        attachment,
                        ..
                    } = &mut app.modal
                        && message.account == account
                        && message.peer == peer
                        && message.id == id
                        && message.forward_path == forward_path
                    {
                        if let Some(a) = message
                            .attachments
                            .iter_mut()
                            .find(|a| a.element_id == element)
                        {
                            a.path = path.clone();
                            a.warning = warning.clone();
                        }
                        if attachment.element_id == element {
                            attachment.path = path.clone();
                            attachment.warning = warning.clone();
                        }
                    }
                    if let Modal::Forward { messages, .. } = &mut app.modal {
                        update_forward_media(
                            messages,
                            &account,
                            &peer,
                            &id,
                            &element,
                            &forward_path,
                            &resource,
                        );
                    }
                    if let Some(parent) = app.modal_return.as_mut()
                        && let Modal::Forward { messages, .. } = parent.as_mut()
                    {
                        update_forward_media(
                            messages,
                            &account,
                            &peer,
                            &id,
                            &element,
                            &forward_path,
                            &resource,
                        );
                    }
                    if open {
                        controller.action(app, Action::Open(path.clone()));
                    }
                    if forward_path.is_empty()
                        && let Some(chat) = app.chats.get_mut(&peer)
                        && let Some(m) = chat.messages.iter_mut().find(|m| m.id == id)
                        && let Some(a) = m.attachments.iter_mut().find(|a| a.element_id == element)
                    {
                        a.path = path;
                        a.warning = warning;
                        m.touch();
                    }
                }
                Err(e) => {
                    if matches!(&app.modal, Modal::Preview { message, attachment, .. } if message.account == account && message.peer == peer && message.id == id && message.forward_path==forward_path && attachment.element_id == element)
                    {
                        app.notice = format!("图片加载失败：{e}");
                    } else if open
                        || matches!(&app.modal,Modal::Forward{path,..} if path==&forward_path)
                        || app.modal_return.as_deref().is_some_and(
                            |modal| matches!(modal,Modal::Forward{path,..} if path==&forward_path),
                        )
                    {
                        app.operation_error(&e);
                    }
                }
            }
        }
        Work::Outbox(result) => match result {
            Ok(items) => {
                for (r, d) in items {
                    app.receipts.entry(r.request_id.clone()).or_insert((r, d));
                }
                controller.reconcile(app);
            }
            Err(e) => app.operation_error(&e),
        },
        Work::Error(message) => app.operation_error(message),
        Work::StageFailed { epoch, message } => {
            if epoch == app.epoch {
                app.staging = false;
                app.operation_error(message);
            }
        }
    }
}
