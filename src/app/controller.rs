use super::work::{RequestContext, Work, WorkSender, apply_receipt, apply_work};
use super::*;
pub struct Controller {
    pub clipboard: media::ClipboardAccess,
    pub backend: Option<Backend>,
    pub store: Store,
    pub paths: Paths,
    pub tx: mpsc::Sender<Work>,
    pub connecting: bool,
    pub status_pending: bool,
    pub status_again: bool,
}
impl Controller {
    pub(crate) fn ready_backend(&self, app: &App) -> Option<Backend> {
        self.backend.clone().filter(|_| app.ready)
    }
    pub(crate) fn sender(&self, app: &App) -> WorkSender {
        WorkSender {
            sender: self.tx.clone(),
            scope: RequestContext {
                account: app.account.clone(),
                view: app.context,
                native: self.backend.as_ref().map(|b| (b.id(), app.epoch.clone())),
            },
        }
    }
    pub fn connect(&mut self, events: mpsc::Sender<BackendEvent>) {
        if self.connecting || self.backend.is_some() {
            return;
        }
        self.connecting = true;
        let path = self.paths.socket();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = Backend::connect(&path, events).await;
            let _ = tx.send(Work::Connected(result)).await;
        });
    }
    pub fn status(&mut self) {
        if self.status_pending {
            self.status_again = true;
            return;
        }
        if let Some(b) = self.backend.clone() {
            self.status_pending = true;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(Work::Connection {
                        id: b.id(),
                        epoch: None,
                        work: Box::new(Work::Status(b.call("host.status", json!({})).await)),
                    })
                    .await;
            });
        }
    }
    pub fn drain_updates(
        &mut self,
        app: &mut App,
        native: &mut mpsc::Receiver<BackendEvent>,
        work: &mut mpsc::Receiver<Work>,
    ) {
        let store = self.store.clone();
        for _ in 0..64 {
            let Ok(result) = work.try_recv() else {
                break;
            };
            apply_work(app, self, &store, result);
        }
        for _ in 0..64 {
            let Ok(event) = native.try_recv() else {
                break;
            };
            self.event(app, event);
        }
    }
    pub fn invalidate_history(&self, app: &mut App) {
        for chat in app.chats.values_mut() {
            chat.quotes.retain(|_, q| matches!(q, QuoteState::Ready(_)));
            chat.loaded = false;
            chat.read = ReadState::default();
            if matches!(chat.unread, UnreadState::Loading { .. }) {
                chat.unread = UnreadState::Uncaptured;
            }
            if chat.loading {
                chat.pending_history = Some(chat.history_position());
            }
        }
        if let Some(peer) = app.selected_peer.clone() {
            let position = app
                .chats
                .get(&peer)
                .map(Chat::history_position)
                .unwrap_or(HistoryPoint::Latest);
            self.history(app, peer, position);
        }
    }
    pub fn event(&mut self, app: &mut App, event: BackendEvent) {
        match event {
            BackendEvent::Connection { id, event } => {
                if self
                    .backend
                    .as_ref()
                    .is_some_and(|backend| backend.id() == id)
                {
                    self.event(app, *event);
                }
            }
            BackendEvent::Disconnected(reason) => {
                self.backend = None;
                self.status_pending = false;
                self.status_again = false;
                app.connected = false;
                app.ready = false;
                if app.pending_account.take().is_some() {
                    app.staging = false;
                }
                app.syncing = false;
                app.reset_native_pending();
                app.close_lifecycle_modals();
                app.notice = reason;
                app.dirty = true;
            }
            BackendEvent::Native(event) => {
                if event.epoch != app.epoch || event.account != app.account {
                    app.sync_needed = true;
                    self.status();
                    return;
                }
                if event.seq <= app.seq {
                    return;
                }
                if event.seq > app.seq + 1 {
                    app.sync_needed = true;
                    app.sync_urgent = true;
                    self.invalidate_history(app);
                }
                app.seq = event.seq;
                match event.kind.as_str() {
                    "notification" => {
                        if let Some(batch) = event.notification {
                            for item in batch.items.into_iter().take(64) {
                                if app.notifications.len() < 64
                                    || app.notifications.contains_key(&item.key)
                                {
                                    app.notifications.insert(item.key.clone(), item);
                                }
                            }
                        }
                    }
                    "avatars.invalidated" => {
                        for avatar in app.avatars.values().filter_map(|v| v.as_ref().ok()) {
                            app.media_invalidations.push(avatar.path.clone());
                        }
                        std::rc::Rc::make_mut(&mut app.avatars).clear();
                        app.avatar_pending.clear();
                        app.dirty = true;
                    }
                    "permissions.invalidated" => {
                        if let Some(peer) = app.selected_peer.clone()
                            && let Some(c) = app.chats.get_mut(&peer)
                        {
                            c.permission_at = None;
                            c.permission_generation += 1;
                        }
                    }
                    "recent.invalidated" => app.sync_needed = true,
                    "stickers.invalidated" => {
                        app.sticker_errors.clear();
                        if matches!(app.modal, Modal::EmojiPicker { .. }) {
                            app.modal = Modal::None;
                            app.pending_dialog = None;
                            app.notice = "QQ 表情资源已更新，请重新打开表情选择器".into();
                            app.dirty = true;
                        }
                    }
                    "messages.invalidated" => {
                        app.sync_needed = true;
                        self.invalidate_history(app);
                    }
                    "message.recalled" => {
                        if let Some(peer) = event.message_peer {
                            if matches!(&app.modal,Modal::Preview{message,..} if message.peer==peer && message.seq==event.message_seq)
                            {
                                app.modal = Modal::None;
                            }
                            let mut changed = vec![];
                            if let Some(chat) = app.chats.get_mut(&peer) {
                                chat.quotes
                                    .retain(|_, q| !matches!(q, QuoteState::Loading(_)));
                                for quote in chat.quotes.values_mut() {
                                    if let QuoteState::Ready(message) = quote
                                        && message.seq == event.message_seq
                                    {
                                        message.retract();
                                    }
                                }
                                for message in &mut chat.messages {
                                    if message.seq == event.message_seq
                                        && (!message.is_notice() || message.recalled)
                                    {
                                        message.retract();
                                        changed.push(message.clone());
                                    }
                                }
                                if chat
                                    .draft
                                    .reply_to
                                    .as_ref()
                                    .is_some_and(|id| changed.iter().any(|m| &m.id == id))
                                {
                                    chat.draft.reply_to = None;
                                    chat.changed();
                                }
                            }
                            let store = self.store.clone();
                            let tx = self.sender(app);
                            let account = app.account.clone();
                            let seq = event.message_seq;
                            tokio::spawn(async move {
                                if let Err(e) = store.save_messages(changed).await {
                                    let _ = tx.send_local(Work::Error(e.to_string())).await;
                                }
                                match store.recall(account.clone(), peer.clone(), seq).await {
                                    Ok(messages) => {
                                        let _ = tx.send(Work::Recalled { peer, messages }).await;
                                    }
                                    Err(e) => {
                                        let _ = tx.send_local(Work::Error(e.to_string())).await;
                                    }
                                }
                            });
                            app.dirty = true;
                        }
                    }
                    "notices.updated" => {
                        let mut changed = vec![];
                        for update in event.messages {
                            if let Some(message) = app
                                .chats
                                .get_mut(&update.peer)
                                .and_then(|c| c.messages.iter_mut().find(|m| m.id == update.id))
                                && message.is_notice()
                                && message.recalled == update.recalled
                            {
                                message.text = update.text;
                                message.parts = update.parts;
                                message.notice_names = update.notice_names;
                                message.touch();
                                changed.push(message.clone());
                            }
                        }
                        let store = self.store.clone();
                        tokio::spawn(async move {
                            let _ = store.save_messages(changed).await;
                        });
                        app.dirty = true;
                    }
                    "reactions.updated" => {
                        let mut changed = vec![];
                        for update in event.messages {
                            if let Some(message) = app
                                .chats
                                .get_mut(&update.peer)
                                .and_then(|c| c.messages.iter_mut().find(|m| m.id == update.id))
                            {
                                message.reactions = update.reactions.clone();
                                message.touch();
                                changed.push(message.clone());
                            }
                            if let Modal::Reactions { message, .. } = &mut app.modal
                                && message.peer == update.peer
                                && message.id == update.id
                            {
                                message.reactions = update.reactions;
                            }
                        }
                        let store = self.store.clone();
                        tokio::spawn(async move {
                            let _ = store.save_messages(changed).await;
                        });
                        app.dirty = true;
                    }
                    "messages" => {
                        for message in &event.messages {
                            if let Some(notice) = message.reaction_reference() {
                                self.refresh_reactions(app, message.peer.clone(), notice.seq);
                            }
                            if let Modal::Reactions {
                                message: visible, ..
                            } = &mut app.modal
                                && visible.peer == message.peer
                                && visible.id == message.id
                            {
                                visible.reactions = message.reactions.clone();
                            }
                        }
                        let messages = event.messages;
                        app.dirty |= messages
                            .iter()
                            .any(|m| Some(&m.peer) == app.selected_peer.as_ref());
                        let mut peers: HashMap<Peer, Vec<Message>> = HashMap::new();
                        for message in &messages {
                            peers
                                .entry(message.peer.clone())
                                .or_default()
                                .push(message.clone());
                        }
                        for (peer, messages) in peers {
                            if let Some(c) = app.chats.get_mut(&peer)
                                && (c.loaded || app.selected_peer.as_ref() == Some(&peer))
                            {
                                c.receive(messages);
                            }
                        }
                        let store = self.store.clone();
                        let tx = self.sender(app);
                        tokio::spawn(async move {
                            if let Err(e) = store.save_messages(messages).await {
                                let _ = tx.send_local(Work::Error(e.to_string())).await;
                            }
                        });
                        app.sync_needed = true;
                    }
                    "send.updated" => {
                        if let Some(receipt) = event.receipt {
                            apply_receipt(app, &self.store, self.sender(app), receipt);
                            app.dirty = true;
                        }
                    }
                    "status" => self.status(),
                    _ => {}
                }
            }
        }
    }
    pub fn quote(&self, app: &mut App, source: Message, jump: bool) {
        let Some(key) = source.quote_key() else {
            return;
        };
        if source.account != app.account {
            return;
        }
        let peer = source.peer.clone();
        if jump {
            app.chats.entry(peer.clone()).or_default().quote_jump =
                Some((key.clone(), source.id.clone()));
        }
        let ready = app.chats.get(&peer).and_then(|chat| {
            chat.messages
                .iter()
                .find(|m| {
                    source.reaction_notice.is_none() && Some(&m.id) == source.reply_to.as_ref()
                })
                .cloned()
                .or_else(|| match chat.quotes.get(&key) {
                    Some(QuoteState::Ready(message)) => Some(message.as_ref().clone()),
                    _ => None,
                })
        });
        if let Some(original) = ready {
            if jump {
                self.jump(app, peer, original.id);
            }
            return;
        }
        if app.chats.get(&peer).is_some_and(|chat| {
            matches!(chat.quotes.get(&key), Some(QuoteState::Loading(_)))
                || (!jump && matches!(chat.quotes.get(&key), Some(QuoteState::Missing)))
        }) {
            return;
        }
        if app
            .chats
            .values()
            .flat_map(|c| c.quotes.values())
            .filter(|q| matches!(q, QuoteState::Loading(_)))
            .count()
            >= 8
        {
            if jump {
                app.notice = "正在等待引用读取…".into();
            }
            return;
        }
        let chat = app.chats.entry(peer.clone()).or_default();
        if chat.quotes.len() >= 256
            && let Some(old) = chat
                .quotes
                .iter()
                .find(|(_, q)| !matches!(q, QuoteState::Loading(_)))
                .map(|(key, _)| key.clone())
        {
            chat.quotes.remove(&old);
        }
        let token = uuid::Uuid::new_v4();
        chat.quotes.insert(key.clone(), QuoteState::Loading(token));
        if jump {
            app.notice = "正在读取引用原消息…".into();
        }
        let backend = self.ready_backend(app);
        let store = self.store.clone();
        let tx = self.sender(app);
        tokio::spawn(async move {
            let account = source.account.clone();
            let result = async {
                let cached = store
                    .quoted_message(
                        account.clone(),
                        peer.clone(),
                        source
                            .reply_to
                            .as_ref()
                            .filter(|_| source.reaction_notice.is_none())
                            .cloned(),
                        source.reference_seq(),
                    )
                    .await?;
                if cached.as_ref().is_some_and(|m| m.recalled) {
                    return Ok(cached);
                }
                let Some(backend) = backend else {
                    return Ok(cached);
                };
                let value = if let Some(reference) = source.reaction_reference() {
                    backend
                        .call::<Value>(
                            "messages.sequence",
                            json!({"account":account,"peer":peer,"seq":reference.seq}),
                        )
                        .await?
                } else {
                    backend
                        .call::<Value>(
                            "messages.quote",
                            json!({"account":account,"peer":peer,"id":source.id}),
                        )
                        .await?
                };
                let messages: Vec<Message> = serde_json::from_value(value["messages"].clone())?;
                let original = messages
                    .into_iter()
                    .find(|m| m.account == account && m.peer == peer);
                if let Some(original) = original {
                    store.save_messages(vec![original.clone()]).await?;
                    store
                        .quoted_message(account.clone(), peer.clone(), Some(original.id), None)
                        .await
                } else {
                    Ok(cached)
                }
            }
            .await;
            let _ = tx
                .send(Work::Quote {
                    peer,
                    key,
                    token,
                    result,
                })
                .await;
        });
    }
    pub(crate) fn activate_notification(
        &mut self,
        app: &mut App,
        target: crate::notifications::Target,
    ) {
        if target.account != app.account || target.id.is_empty() || target.id == "0" {
            return;
        }
        app.modal = Modal::None;
        app.mode = Mode::Normal;
        app.focus = Focus::Messages;
        self.action(app, Action::Select(target.peer.clone()));
        self.jump(app, target.peer, target.id);
    }
    pub(crate) fn jump(&self, app: &mut App, peer: Peer, id: String) {
        let chat = app.chats.entry(peer.clone()).or_default();
        if chat.jump_back.len() >= 20 {
            chat.jump_back.remove(0);
        }
        chat.jump_back
            .push((chat.viewport.clone(), chat.selected.clone()));
        chat.quote_jump = None;
        chat.viewport = Viewport {
            anchor: Some(id.clone()),
            line: 0,
            follow: false,
        };
        chat.selected = Some(id.clone());
        if app.ready && self.backend.is_some() {
            self.history(app, peer, HistoryPoint::Around(id));
        } else {
            let chat = app.chats.get_mut(&peer).unwrap();
            chat.loaded = false;
            chat.messages.clear();
            self.load_chat(app, &peer);
        }
        app.dirty = true;
    }
    pub(crate) fn capture_unread(&self, app: &mut App, peer: &Peer) {
        if !app.ready || !matches!(app.chats[peer].unread, UnreadState::Uncaptured) {
            return;
        }
        if !peer.is_group() || !app.capabilities.iter().any(|c| c == "unread_anchor") {
            app.chats.get_mut(peer).unwrap().unread = UnreadState::Ready(None);
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let token = uuid::Uuid::new_v4();
        app.chats.get_mut(peer).unwrap().unread = UnreadState::Loading { token, jump: false };
        let tx = self.sender(app);
        let account = app.account.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let result = async {
                let value: Value = backend
                    .call(
                        "conversations.unread",
                        json!({"account":account,"peer":peer}),
                    )
                    .await?;
                Ok(value["id"].as_str().map(str::to_owned))
            }
            .await;
            let _ = tx
                .send(Work::UnreadTarget {
                    peer,
                    token,
                    result,
                })
                .await;
        });
    }
    pub fn history(&self, app: &mut App, peer: Peer, position: HistoryPoint) {
        if !chat_supported(&peer) {
            return;
        }
        if let Some(b) = self.backend.clone() {
            let tx = self.sender(app);
            let store = self.store.clone();
            let account = app.account.clone();
            let c = app.chats.entry(peer.clone()).or_default();
            if c.loading {
                if matches!(position, HistoryPoint::Latest | HistoryPoint::Around(_)) {
                    c.pending_history = Some(position);
                }
                return;
            }
            c.loading = true;
            c.generation += 1;
            let generation = c.generation;
            tokio::spawn(async move {
                let mut params = json!({"account":account,"peer":peer,"limit":40});
                match &position {
                    HistoryPoint::Latest => {}
                    HistoryPoint::Before(id) => params["before"] = json!(id),
                    HistoryPoint::After(id) => params["after"] = json!(id),
                    HistoryPoint::Around(id) => params["around"] = json!(id),
                }
                let result = b.call::<HistoryPage>("messages.history", params).await;
                if let Ok(page) = &result
                    && let Err(e) = store.save_messages(page.messages.clone()).await
                {
                    let _ = tx.send(Work::Error(e.to_string())).await;
                }
                let _ = tx
                    .send(Work::History {
                        peer,
                        generation,
                        position,
                        result,
                    })
                    .await;
            });
        }
    }
    pub fn sticker_preview(&self, app: &mut App, token: uuid::Uuid, sticker: Sticker) {
        if sticker.kind != "sticker"
            || !sticker.path.is_empty()
            || app
                .sticker_errors
                .contains_key(&(token, sticker.id.clone()))
            || app.sticker_previews.len() >= 4
            || !app.sticker_previews.insert((token, sticker.id.clone()))
        {
            return;
        }
        let Some(backend) = self.ready_backend(app) else {
            app.sticker_previews.remove(&(token, sticker.id));
            return;
        };
        let tx = self.sender(app);
        let account = app.account.clone();
        tokio::spawn(async move {
            let result = backend
                .call::<Value>(
                    "stickers.resolve",
                    json!({"account":account,"id":sticker.id}),
                )
                .await
                .and_then(|value| {
                    value["path"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("表情路径缺失"))
                });
            let _ = tx
                .send_local(Work::StickerPreview {
                    token,
                    id: sticker.id,
                    result,
                })
                .await;
        });
    }
    pub(crate) fn refresh_reactions(&self, app: &mut App, peer: Peer, seq: String) {
        if !peer.is_group()
            || app.reaction_refreshes.len() >= 8
            || !app.reaction_refreshes.insert((peer.clone(), seq.clone()))
        {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            app.reaction_refreshes.remove(&(peer, seq));
            return;
        };
        let tx = self.sender(app);
        let account = app.account.clone();
        tokio::spawn(async move {
            // QQ can deliver the gray tip before updating the original message.
            for (attempt, delay) in [0, 250, 750, 1500].into_iter().enumerate() {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if !backend.is_alive() {
                    break;
                }
                let result = backend
                    .call(
                        "reactions.refresh",
                        json!({"account":account,"peer":peer,"seq":seq}),
                    )
                    .await;
                if tx
                    .send(Work::ReactionRefresh {
                        peer: peer.clone(),
                        seq: seq.clone(),
                        complete: attempt == 3,
                        result,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    pub fn action(&self, app: &mut App, action: Action) {
        let tx = self.sender(app);
        let store = self.store.clone();
        let clipboard = self.clipboard.clone();
        let backend = self.backend.clone();
        let account = app.account.clone();
        let peer = app.selected_peer.clone();
        let epoch = app.epoch.clone();
        match action {
            Action::EmojiCatalog(purpose) => {
                app.sticker_errors.clear();
                if !app.capabilities.contains("stickers") {
                    app.notice = "当前 QQ 宿主未提供表情目录接口".into();
                    return;
                }
                let Some(backend) = self.ready_backend(app) else {
                    app.notice = "QQ 尚未就绪".into();
                    return;
                };
                let token = app.begin_dialog("读取 QQ 表情收藏…");
                tokio::spawn(async move {
                    let result = backend
                        .call::<Value>("stickers.list", json!({"account":account,"favorites":matches!(purpose,EmojiPurpose::Send)}))
                        .await
                        .and_then(|v| {
                            serde_json::from_value(v["items"].clone()).map_err(Into::into)
                        });
                    let _ = tx
                        .send(Work::EmojiCatalog {
                            token,
                            purpose,
                            result,
                        })
                        .await;
                });
            }
            Action::ChooseSticker(sticker) => {
                if !app.draft_editable() {
                    return;
                }
                let Some(peer) = peer else {
                    return;
                };
                if sticker.kind == "face" {
                    if peer.is_device() {
                        app.notice = "设备会话不支持 QQ 表情，请选择收藏图片".into();
                        return;
                    }
                    let chat = app.chats.get_mut(&peer).unwrap();
                    chat.draft.attachments.push(Attachment {
                        kind: "face".into(),
                        name: sticker.name,
                        path: sticker.path,
                        element_id: sticker.emoji_id,
                        ..Default::default()
                    });
                    chat.changed();
                    app.mode = Mode::Insert;
                    app.focus = Focus::Messages;
                } else if let Some(backend) = self.ready_backend(app) {
                    let token = app.begin_dialog("读取表情图片…");
                    app.chats.get_mut(&peer).unwrap().composer_pending += 1;
                    let cache = self.paths.cache.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let value: Value = backend
                                .call(
                                    "stickers.resolve",
                                    json!({"account":account,"id":sticker.id}),
                                )
                                .await?;
                            media::sticker_attachment(
                                std::path::Path::new(
                                    value["path"]
                                        .as_str()
                                        .ok_or_else(|| anyhow!("表情路径缺失"))?,
                                ),
                                Some(sticker.name),
                            )
                            .await?
                            .pin_cached(&cache)
                        }
                        .await;
                        let _ = tx
                            .send_local(Work::StickerChosen {
                                token,
                                peer,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Reactions(message) | Action::SetReaction { message, .. }
                if !message.peer.is_group() || message.recalled =>
            {
                app.notice = "仅支持对未撤回的群消息添加表情回应".into();
            }
            Action::Reactions(message) => {
                if !app.capabilities.contains("reactions") {
                    app.notice = "当前 QQ 宿主未提供表情回应接口".into();
                    return;
                }
                let Some(backend) = self.ready_backend(app) else {
                    app.notice = "QQ 尚未就绪".into();
                    return;
                };
                let token = app.begin_dialog("读取表情回应…");
                tokio::spawn(async move {
                    let result = backend
                        .call::<HistoryPage>(
                            "messages.get",
                            json!({"account":account,"peer":message.peer,"ids":[message.id]}),
                        )
                        .await
                        .and_then(|page| {
                            page.messages
                                .into_iter()
                                .next()
                                .ok_or_else(|| anyhow!("消息不可用"))
                        });
                    let _ = tx.send(Work::Reactions { token, result }).await;
                });
            }
            Action::SetReaction {
                message,
                reaction,
                set,
            } => {
                let Some(backend) = self.ready_backend(app) else {
                    app.notice = "QQ 尚未就绪".into();
                    return;
                };
                let token = app.begin_dialog("提交表情回应…");
                tokio::spawn(async move {
                    let result=async {
                        let _:Value=backend.call("reactions.set",json!({"account":account,"epoch":epoch,"peer":message.peer,"id":message.id,"emoji_id":reaction.emoji_id,"emoji_type":reaction.emoji_type,"set":set})).await?;
                        let page:HistoryPage=backend.call("messages.get",json!({"account":account,"peer":message.peer,"ids":[message.id]})).await?;
                        page.messages.into_iter().next().ok_or_else(||anyhow!("回应已提交，消息状态待核对"))
                    }.await;
                    let _ = tx.send(Work::Reactions { token, result }).await;
                });
            }
            Action::ReactionMembers {
                message,
                reaction,
                cookie,
            } => {
                let Some(backend) = self.ready_backend(app) else {
                    return;
                };
                let token = app.begin_dialog("读取回应成员…");
                tokio::spawn(async move {
                    let result=backend.call("reactions.members",json!({"account":account,"peer":message.peer,"id":message.id,"emoji_id":reaction.emoji_id,"emoji_type":reaction.emoji_type,"cookie":cookie.unwrap_or_default()})).await;
                    let _ = tx
                        .send(Work::ReactionMembers {
                            token,
                            message,
                            reaction,
                            result,
                        })
                        .await;
                });
            }
            Action::Forward { root, path } => {
                if path.len() == 1 {
                    app.forward_parents.clear();
                }
                if root.account != app.account {
                    return;
                }
                let Some(backend) = self.ready_backend(app) else {
                    app.notice = "连接 QQ 后可查看合并转发内容".into();
                    return;
                };
                let token = app.begin_dialog("读取合并转发聊天记录…");
                tokio::spawn(async move {
                    let result = backend
                        .call(
                            "messages.forward",
                            json!({"account":account,"peer":root.peer,"path":path}),
                        )
                        .await;
                    let _ = tx
                        .send(Work::Forward {
                            token,
                            root,
                            path,
                            result,
                        })
                        .await;
                });
            }
            Action::JumpQuote(source) => self.quote(app, source, true),
            Action::JumpBack => {
                if let Some(peer) = peer
                    && let Some(chat) = app.chats.get_mut(&peer)
                    && let Some((viewport, selected)) = chat.jump_back.pop()
                {
                    chat.viewport = viewport;
                    chat.selected = selected;
                    chat.quote_jump = None;
                    let position = chat.history_position();
                    if app.ready && self.backend.is_some() {
                        self.history(app, peer, position);
                    } else {
                        let chat = app.chats.get_mut(&peer).unwrap();
                        chat.loaded = false;
                        chat.messages.clear();
                        self.load_chat(app, &peer);
                    }
                } else {
                    app.notice = "没有可返回的阅读位置".into();
                }
            }
            Action::Quit => app.quit = true,
            Action::FirstUnread => {
                if app.ready && !app.capabilities.contains("unread_anchor") {
                    app.notice = "当前宿主不支持未读定位，请重启对应宿主".into();
                    return;
                }
                let Some(peer) = peer else {
                    return;
                };
                app.chats.entry(peer.clone()).or_default();
                self.capture_unread(app, &peer);
                app.focus = Focus::Messages;
                match &mut app.chats.get_mut(&peer).unwrap().unread {
                    UnreadState::Loading { jump, .. } => {
                        *jump = true;
                        app.notice = "正在定位最早未读消息…".into();
                    }
                    UnreadState::Ready(Some(id)) => {
                        let id = id.clone();
                        self.jump(app, peer, id);
                    }
                    UnreadState::Ready(None) => app.notice = "当前会话没有可定位的未读起点".into(),
                    UnreadState::Failed(error) => {
                        let error = format!("读取未读起点：{error}");
                        app.operation_error(error);
                    }
                    UnreadState::Uncaptured => app.notice = "QQ 原生服务未就绪".into(),
                }
            }
            Action::Select(peer) => {
                let changed = app.selected_peer.as_ref() != Some(&peer);
                if let Some(c) = app.chats.get_mut(&peer) {
                    c.last_used = Instant::now();
                }
                if app.selected_peer.as_ref() != Some(&peer) {
                    if let Some(chat) = app.chat_mut() {
                        chat.quote_jump = None;
                    }
                    app.pending_dialog = None;
                    app.modal = Modal::None;
                }
                if app.ready
                    && find_conversation(&app.conversations, &peer).is_none()
                    && let Some(b) = self.backend.clone()
                {
                    let tx = self.sender(app);
                    let p = peer.clone();
                    let a = account.clone();
                    tokio::spawn(async move {
                        match b
                            .call::<Value>(
                                "conversations.open",
                                json!({"account":a,"epoch":epoch,"peer":p}),
                            )
                            .await
                        {
                            Ok(_) => {
                                let _ = tx.send(Work::Refresh).await;
                            }
                            Err(e) => {
                                let _ = tx.send(Work::Error(e.to_string())).await;
                            }
                        }
                    });
                }
                app.sidebar_cursor = Some(peer.clone());
                app.selected_peer = Some(peer.clone());
                let chat = app.chats.entry(peer.clone()).or_default();
                if changed {
                    chat.unread = UnreadState::Uncaptured;
                }
                app.chats.get_mut(&peer).unwrap().read.confirmed.clear();
                self.load_chat(app, &peer);
            }
            Action::MarkRead(conversation) => {
                if let Some(b) = self.ready_backend(app) {
                    let epoch = app.epoch.clone();
                    tokio::spawn(async move {
                        let result=b.call::<Value>("conversations.read",json!({"account":account,"epoch":epoch,"id":conversation.id,"peer":conversation.peer})).await.map(|_|());
                        let _ = tx.send(Work::ConversationRead { result }).await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Folder => {
                if let (Some(b), Some(peer)) = (self.ready_backend(app), peer) {
                    let token = app.begin_dialog("读取会话分组…");
                    tokio::spawn(async move {
                        let result = b
                            .call::<Snapshot>(
                                "conversations.children",
                                json!({"account":account,"peer":peer}),
                            )
                            .await
                            .map(|s| s.conversations);
                        let _ = tx
                            .send(Work::Folder {
                                token,
                                peer,
                                result,
                            })
                            .await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Profile(uid) => {
                if let (Some(b), Some(peer)) = (self.ready_backend(app), peer) {
                    let token = app.begin_dialog("读取联系人资料…");
                    tokio::spawn(async move {
                        let result = b
                            .call("profiles.info", json!({"account":account,"uid":uid}))
                            .await;
                        let _ = tx
                            .send(Work::Profile {
                                token,
                                peer,
                                result,
                            })
                            .await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Snapshot => {
                if app.syncing {
                    return;
                }
                if let Some(b) = backend {
                    app.syncing = true;
                    app.sync_needed = false;
                    app.sync_urgent = false;
                    tokio::spawn(async move {
                        let result = b
                            .call("conversations.snapshot", json!({"account":account}))
                            .await;
                        let _ = tx.send(Work::Snapshot(result)).await;
                    });
                }
            }
            Action::History(position) => {
                if let Some(peer) = peer {
                    self.history(app, peer, position);
                }
            }
            Action::RestoreDraft(receipt, draft) => {
                if receipt.account != account || !chat_supported(&receipt.peer) {
                    return;
                }
                let peer = receipt.peer.clone();
                self.action(app, Action::Select(peer.clone()));
                let version = app.chats[&peer].draft_version;
                app.notice = "正在读取目标会话草稿…".into();
                tokio::spawn(async move {
                    let existing = store.draft(account.clone(), peer.clone()).await;
                    let _ = tx
                        .send_local(Work::RestoreDraft {
                            peer,
                            version,
                            draft,
                            state: receipt.state.to_string(),
                            existing,
                        })
                        .await;
                });
            }
            Action::Send => {
                if peer.as_ref().is_none_or(|p| !chat_supported(p)) {
                    app.notice = "此入口不是聊天会话，按 Enter 展开".into();
                    return;
                }
                if !app.ready || app.staging {
                    app.notice = "QQ 尚未就绪".into();
                    return;
                }
                if !app.draft_editable() {
                    return;
                }
                if let (Some(b), Some(peer)) = (backend, peer) {
                    if app.chats[&peer].draft_saving.is_some() {
                        app.queued_send = Some((peer.clone(), app.chats[&peer].draft_version));
                        app.staging = true;
                        app.notice = "等待草稿保存后发送".into();
                        return;
                    }
                    if app.chats[&peer].composer_pending > 0 {
                        app.notice = "附件或剪贴板仍在处理中".into();
                        return;
                    }
                    let draft = app.chats[&peer].draft.clone();
                    let epoch = app.epoch.clone();
                    if draft.text.is_empty() && draft.attachments.is_empty() {
                        return;
                    }
                    let receipt = Receipt {
                        request_id: uuid::Uuid::new_v4().to_string(),
                        account: account.clone(),
                        peer: peer.clone(),
                        state: "submitting".into(),
                        ..Default::default()
                    };
                    app.staging = true;
                    tokio::spawn(async move {
                        if let Err(e) = store.stage_send(receipt.clone(), draft.clone()).await {
                            let _ = tx
                                .send_local(Work::StageFailed {
                                    epoch: epoch.clone(),
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                        let (proceed, accepted) = oneshot::channel();
                        let _ = tx
                            .send_local(Work::Staged {
                                peer: peer.clone(),
                                receipt: receipt.clone(),
                                draft: draft.clone(),
                                epoch: epoch.clone(),
                                proceed,
                            })
                            .await;
                        if accepted.await.is_err() {
                            let cancelled = Receipt {
                                state: "failed".into(),
                                error: Some("发送上下文已变更，消息未提交".into()),
                                ..receipt
                            };
                            let _ = store.receipt(cancelled.clone()).await;
                            let _ = tx.receipt(Ok(cancelled)).await;
                            return;
                        }
                        let segments = draft_segments(&draft);
                        let result=b.call::<Receipt>("messages.send",json!({"account":account,"epoch":epoch,"peer":peer,"request_id":receipt.request_id,"text":draft.text,"segments":segments,"reply_to":draft.reply_to,"attachments":draft.attachments})).await;
                        let result = match result {
                            Ok(r) => Ok(r),
                            Err(e) => Ok(Receipt {
                                state: "unknown".into(),
                                error: Some(e.to_string()),
                                ..receipt
                            }),
                        };
                        if let Ok(r) = &result {
                            let _ = store.receipt(r.clone()).await;
                        }
                        let _ = tx.receipt(result).await;
                    });
                }
            }
            Action::Copy(text) => {
                tokio::spawn(async move {
                    if let Err(e) = clipboard.copy(&text).await {
                        let _ = tx.send(Work::Error(e.to_string())).await;
                    }
                });
            }
            Action::Paste => {
                if !app.draft_editable() {
                    return;
                }
                if let Some(peer) = peer {
                    let chat = app.chats.entry(peer.clone()).or_default();
                    if chat.composer_pending >= 2 {
                        app.notice = "附件仍在处理中".into();
                        return;
                    }
                    chat.composer_pending += 1;
                    let generation = chat.composer_generation;
                    let cache = self.paths.cache.clone();
                    tokio::spawn(async move {
                        let result = clipboard.paste(&cache).await;
                        let _ = tx
                            .send_local(Work::Clipboard {
                                peer,
                                generation,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Attach { file, sticker } => {
                if !app.draft_editable() {
                    return;
                }
                if let Some(peer) = peer {
                    let chat = app.chats.entry(peer.clone()).or_default();
                    if chat.composer_pending >= 2 {
                        app.notice = "附件仍在处理中".into();
                        return;
                    }
                    chat.composer_pending += 1;
                    let generation = chat.composer_generation;
                    let cache = self.paths.cache.clone();
                    tokio::spawn(async move {
                        let result = if sticker {
                            media::sticker_attachment(std::path::Path::new(&file), None).await
                        } else {
                            media::attachment(std::path::Path::new(&file)).await
                        }
                        .and_then(|a| a.pin_cached(&cache));
                        let _ = tx
                            .send_local(Work::Attachment {
                                peer,
                                generation,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Members(mention) => {
                if let (Some(b), Some(peer)) = (self.ready_backend(app), peer) {
                    if !peer.is_group() {
                        app.notice = "成员列表仅适用于群聊".into();
                        return;
                    }
                    let token = app.begin_dialog("读取群成员…");
                    tokio::spawn(async move {
                        let result = b
                            .call::<Value>("members.list", json!({"account":account,"peer":peer}))
                            .await
                            .and_then(parse_members);
                        let _ = tx
                            .send(Work::Members {
                                token,
                                peer,
                                mention,
                                result,
                            })
                            .await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Contacts => {
                if let Some(b) = self.ready_backend(app) {
                    let token = app.begin_dialog("读取联系人…");
                    tokio::spawn(async move {
                        let result = b
                            .call::<Value>("contacts.list", json!({"account":account}))
                            .await
                            .and_then(parse_contacts);
                        let _ = tx.send(Work::Contacts { token, result }).await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Search(query) => {
                if let Some(peer) = peer {
                    let Modal::Search { results, index, .. } = &mut app.modal else {
                        return;
                    };
                    results.clear();
                    *index = 0;
                    let token = uuid::Uuid::new_v4();
                    app.pending_dialog = Some(token);
                    tokio::spawn(async move {
                        let result = store
                            .messages(account.clone(), peer.clone(), Some(query.clone()))
                            .await;
                        let _ = tx
                            .send_local(Work::Search {
                                token,
                                peer,
                                query,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Recall(message) => {
                if let Some(b) = backend {
                    tokio::spawn(async move {
                        match b.call::<Value>("messages.recall",json!({"account":message.account,"epoch":epoch,"peer":message.peer,"id":message.id})).await{
                        Ok(_)=>{let _=tx.send(Work::RecallSubmitted{peer:message.peer,id:message.id}).await;},
                        Err(e)=>{let _=tx.send(Work::Error(e.to_string())).await;}
                    }
                    });
                }
            }
            Action::Resolve(message, attachment) => self.resolve(app, message, attachment, true),
            Action::Open(path) => {
                tokio::spawn(async move {
                    if let Err(e) = media::open(&path).await {
                        let _ = tx.send(Work::Error(e.to_string())).await;
                    }
                });
            }
        }
        app.dirty = true;
    }
    pub fn resolve(&self, app: &mut App, message: Message, attachment: Attachment, open: bool) {
        if open && std::path::Path::new(&attachment.path).is_file() {
            self.action(app, Action::Open(attachment.path));
            return;
        }
        let Some(b) = self.ready_backend(app) else {
            if open {
                app.notice = "QQ 原生服务未就绪，暂时无法下载附件".into();
            }
            return;
        };
        let key = message.media_key(&attachment.element_id);
        if let Some(wants_open) = app.pending_images.get_mut(&key) {
            *wants_open |= open;
            return;
        }
        if app.pending_images.len() >= 8 {
            if open {
                app.notice = "媒体请求队列已满，请稍后重试".into();
            }
            return;
        }
        if !open && app.failed_images.contains_key(&key) {
            return;
        }
        app.failed_images.remove(&key);
        app.pending_images.insert(key, open);
        {
            let tx = self.sender(app);
            tokio::spawn(async move {
                let result=b.call::<ResolvedMedia>("media.resolve",json!({"account":message.account,"peer":message.peer,"id":message.id,"element_id":attachment.element_id,"forward_path":message.forward_path})).await;
                let _ = tx
                    .send(Work::Resolved {
                        account: message.account,
                        peer: message.peer,
                        id: message.id,
                        element: attachment.element_id,
                        forward_path: message.forward_path,
                        result,
                        open,
                    })
                    .await;
            });
        }
    }
    pub fn permission(&self, app: &mut App, peer: Peer) {
        if !peer.is_group() || !app.capabilities.contains("speech_permissions") {
            return;
        }
        let Some(b) = self.ready_backend(app) else {
            return;
        };
        let c = app.chats.entry(peer.clone()).or_default();
        if c.permission_pending {
            return;
        }
        c.permission_pending = true;
        let generation = c.permission_generation;
        let tx = self.sender(app);
        let account = app.account.clone();
        tokio::spawn(async move {
            let result = b
                .call(
                    "conversations.permissions",
                    json!({"account":account,"peer":peer}),
                )
                .await;
            let _ = tx
                .send(Work::Permission {
                    peer,
                    generation,
                    result,
                })
                .await;
        });
    }
    pub fn avatar(&self, app: &mut App, uid: String) {
        if !app.capabilities.contains("avatars")
            || app.avatars.contains_key(&uid)
            || app.avatar_pending.contains_key(&uid)
            || app.avatar_pending.len() >= 4
        {
            return;
        }
        let Some(b) = self.ready_backend(app) else {
            return;
        };
        let token = uuid::Uuid::new_v4();
        app.avatar_pending.insert(uid.clone(), token);
        let tx = self.sender(app);
        let account = app.account.clone();
        tokio::spawn(async move {
            let result = b
                .call("avatars.resolve", json!({"account":account,"uid":uid}))
                .await;
            let _ = tx.send(Work::Avatar { uid, token, result }).await;
        });
    }
    pub fn load_chat(&self, app: &mut App, peer: &Peer) {
        self.capture_unread(app, peer);
        self.permission(app, peer.clone());
        if !chat_supported(peer) {
            app.notice = "按 Enter 展开此会话分组".into();
            return;
        }
        let account = app.account.clone();
        let peer = peer.clone();
        let target = peer.clone();
        let version = app.chats[&peer].draft_version;
        let position = app.chats[&peer].history_position();
        let anchor = (!app.chats[&peer].viewport.follow)
            .then(|| app.chats[&peer].viewport.anchor.clone())
            .flatten();
        let tx = self.sender(app);
        let store = self.store.clone();
        tokio::spawn(async move {
            let messages = store
                .cached_window(account.clone(), peer.clone(), anchor)
                .await;
            let draft = store.draft(account.clone(), peer.clone()).await;
            let _ = tx
                .send_local(Work::Cached {
                    peer,
                    version,
                    messages,
                    draft,
                })
                .await;
        });
        let chat = &app.chats[&target];
        if app.ready && !chat.loaded && !chat.loading {
            self.history(app, target, position);
        }
    }
    pub fn autosave(&self, app: &mut App) {
        let tx = self.sender(app);
        for (peer, c) in &mut app.chats {
            if c.draft_loaded
                && c.draft_saving.is_none()
                && !app.staging
                && c.dirty_since
                    .is_some_and(|t| t.elapsed() >= Duration::from_millis(300))
            {
                let version = c.draft_version;
                match self
                    .store
                    .queue_draft(app.account.clone(), peer.clone(), c.draft.clone())
                {
                    Ok(done) => {
                        c.draft_saving = Some(version);
                        let tx = tx.clone();
                        let peer = peer.clone();
                        tokio::spawn(async move {
                            let result = done.await.unwrap_or_else(|e| Err(e.into()));
                            let _ = tx
                                .send_local(Work::DraftSaved {
                                    peer,
                                    version,
                                    result,
                                })
                                .await;
                        });
                    }
                    Err(error) => {
                        app.notice = error.to_string();
                        c.dirty_since = Some(Instant::now() + Duration::from_secs(1));
                    }
                }
            }
        }
    }
    pub async fn flush_drafts(&self, app: &App) -> Result<()> {
        for (peer, chat) in &app.chats {
            if chat.draft_loaded && chat.dirty_since.is_some() {
                self.store
                    .save_draft(app.account.clone(), peer.clone(), chat.draft.clone())
                    .await?;
            }
        }
        Ok(())
    }
    pub fn read_if_visible(&self, app: &mut App, readable: bool) {
        if !app.ready || !app.connected || !app.terminal_focused || !readable {
            return;
        }
        let (Some(b), Some(peer)) = (self.backend.clone(), app.selected_peer.clone()) else {
            return;
        };
        if !chat_supported(&peer) {
            return;
        }
        let Some(chat) = app.chats.get_mut(&peer) else {
            return;
        };
        if !chat.loaded
            || chat.loading
            || chat.has_later
            || matches!(chat.unread, UnreadState::Loading { .. })
            || chat.read.pending.is_some()
            || chat
                .read
                .retry_after
                .is_some_and(|deadline| deadline > Instant::now())
        {
            return;
        }
        let Some(last) = chat.messages.last() else {
            return;
        };
        if chat.read.confirmed == last.id {
            return;
        }
        let last_id = last.id.clone();
        let token = uuid::Uuid::new_v4();
        chat.read.pending = Some(token);
        chat.read.retry_after = None;
        let account = app.account.clone();
        let epoch = app.epoch.clone();
        let tx = self.sender(app);
        tokio::spawn(async move {
            let result = b
                .call::<Value>(
                    "messages.read",
                    json!({"account":account,"epoch":epoch,"peer":peer}),
                )
                .await
                .map(|_| ());
            let _ = tx
                .send(Work::Read {
                    peer,
                    token,
                    last_id,
                    result,
                })
                .await;
        });
    }
    pub fn reconcile(&self, app: &App) {
        if let Some(b) = self.backend.clone() {
            for (receipt, _) in app
                .receipts
                .values()
                .filter(|(r, _)| r.state != "confirmed" && r.state != "failed")
            {
                let b = b.clone();
                let tx = self.sender(app);
                let receipt = receipt.clone();
                tokio::spawn(async move {
                    let result=b.call("messages.receipt",json!({"account":receipt.account,"peer":receipt.peer,"request_id":receipt.request_id,"native_id":receipt.native_id,"message_id":receipt.message_id})).await;
                    let _ = tx.receipt(result).await;
                });
            }
        }
    }
}
pub fn draft_segments(d: &Draft) -> Vec<Value> {
    let mut segments = vec![];
    let mut pos = 0;
    let mut mentions = d.mentions.clone();
    mentions.sort_by_key(|m| m.start);
    for m in mentions {
        if m.start < pos
            || m.end > d.text.len()
            || !d.text.is_char_boundary(m.start)
            || !d.text.is_char_boundary(m.end)
        {
            continue;
        }
        if m.start > pos {
            segments.push(json!({"text":&d.text[pos..m.start]}));
        }
        segments.push(json!({"text":&d.text[m.start..m.end],"uid":m.uid,"uin":m.uin}));
        pos = m.end;
    }
    if pos < d.text.len() {
        segments.push(json!({"text":&d.text[pos..]}));
    }
    segments
}
fn parse_members(value: Value) -> Result<Vec<Member>> {
    serde_json::from_value(value["items"].clone()).context("群成员桥接结构不匹配")
}
fn parse_contacts(value: Value) -> Result<Vec<Conversation>> {
    serde_json::from_value(value["items"].clone()).context("联系人桥接结构不匹配")
}

pub async fn run(paths: Paths) -> Result<()> {
    use crossterm::{
        event::{
            EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, EventStream,
            KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        },
        execute,
    };
    use futures::StreamExt;
    paths.create()?;
    let store = Store::open(&paths.state.join("cache.sqlite3")).await?;
    let mut app = App::new(paths.config()?);
    app.instance_account = paths.account.clone();
    let initial_sidebar = (app.config.sidebar_width, app.config.sidebar_visible);
    if let Some(snapshot) = store.cached_snapshot().await? {
        app.apply_snapshot(snapshot);
    }
    if let Some(saved) = store.setting("ui-state".into(), None).await?
        && let Ok(saved) = serde_json::from_str::<SavedUi>(&saved)
        && saved.account == app.account
    {
        if saved
            .selected_peer
            .as_ref()
            .is_some_and(|p| find_conversation(&app.conversations, p).is_some())
        {
            app.selected_peer = saved.selected_peer;
        }
        for (peer, viewport, selected) in saved.views {
            let chat = app.chats.entry(peer).or_default();
            chat.viewport = viewport;
            chat.selected = selected;
        }
    }
    if !app.account.is_empty() {
        for (mut receipt, draft) in store.outbox(app.account.clone()).await? {
            if matches!(receipt.state.as_str(), "submitting" | "submitted") {
                receipt.state = "unknown".into();
            }
            app.receipts
                .insert(receipt.request_id.clone(), (receipt, draft));
        }
    }
    let (tx, mut work_rx) = mpsc::channel(256);
    let (native_tx, mut native_rx) = mpsc::channel(1024);
    let mut controller = Controller {
        clipboard: Default::default(),
        backend: None,
        store: store.clone(),
        paths: paths.clone(),
        tx,
        connecting: false,
        status_pending: false,
        status_again: false,
    };
    if let Some(peer) = app.selected_peer.clone() {
        controller.load_chat(&mut app, &peer);
    }
    let mut terminal = ratatui::init();
    let _restore = TerminalGuard;
    execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableFocusChange,
        EnableMouseCapture
    )?;
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let mut media = media::Media::new(picker);
    media.owned_cache = Some(paths.cache.clone());
    let input_method =
        crate::ime::InputMethod::start(app.config.auto_switch_ime, app.config.ime_ascii.clone());
    let mut ime_focused = true;
    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_connect = Instant::now() - Duration::from_secs(3);
    let mut last_status = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut last_snapshot = Instant::now() - Duration::from_secs(1);
    let mut notifier = crate::notifications::Notifier::start(&paths)?;
    let mut last_notification_claim = Instant::now();
    let mut window_title = String::new();
    controller.connect(native_tx.clone());
    let terminate = crate::platform::shutdown_signal();
    tokio::pin!(terminate);
    let interface_result: Result<()> = async {
        while !app.quit {
            controller.drain_updates(&mut app, &mut native_rx, &mut work_rx);
            app.bound_windows();
            if let Some(ime) = &input_method {
                ime.update(ime_focused, app.modal.accepts_text()
                    || (!app.modal.is_open() && app.mode == Mode::Insert && app.draft_editable()));
            }
            if let Some(notifier) = notifier.as_ref()
                && !app.notifications.is_empty()
            {
                let pending = std::mem::take(&mut app.notifications)
                    .into_values()
                    .collect();
                if let Err(items) = notifier.submit(pending) {
                    app.notifications
                        .extend(items.into_iter().map(|n| (n.key.clone(), n)));
                }
            } else if notifier.is_none() {
                app.notifications.clear();
            }
            if let Some(notifier) = notifier.as_mut()
                && let Ok(error) = notifier.errors.try_recv()
            {
                app.notice = error;
                app.dirty = true;
            }
            if let Some(notifier) = notifier.as_mut() {
                while let Ok(target) = notifier.activated.try_recv() {
                    controller.activate_notification(&mut app, target);
                }
            }
            if app.dirty || media.has_terminal_output() {
                if !app.dirty {
                    media.flush_retired();
                    continue;
                }
                if app.media_reset {media.clear_context();app.media_reset=false;}
                let title = crate::notifications::title(
                    &app.account,
                    &app.conversations,
                    &app.name(),
                    app.unread_total,
                );
                if title != window_title {
                    execute!(std::io::stdout(), crossterm::terminal::SetTitle(&title))?;
                    window_title = title;
                }
                for path in app.media_invalidations.drain(..) {
                    media.invalidate_path(&path);
                }
                if media.clear_required {
                    media.clear_graphics();
                    // Fullscreen resize invalidates buffers without a cursor-position query.
                    let area = terminal.size()?.into();
                    terminal.resize(area)?;
                    media.clear_required = false;
                }
                let mut work = crate::ui::DrawWork::default();
                terminal.draw(|f| {
                    work = crate::ui::draw(f, &mut app, &mut media);
                })?;
                for uid in work.avatars.drain(..) {
                    controller.avatar(&mut app, uid);
                }
                media.flush_retired();
                media.set_visible(work.images.iter().cloned());
                for key in work.images {
                    media.request(key);
                }
                for (message, attachment) in work.resources {
                    controller.resolve(&mut app, message, attachment, false);
                }
                for (token, sticker) in work.sticker_resources {
                    controller.sticker_preview(&mut app, token, sticker);
                }
                for source in work.quotes {
                    controller.quote(&mut app, source, false);
                }
                app.dirty = work.redraw;
                controller.read_if_visible(&mut app, work.readable);
            }
            tokio::select! {
                event=terminal_events.next()=>match event {
                    Some(Ok(event)) => {
                        match event {
                            TerminalEvent::FocusLost => ime_focused = false,
                            TerminalEvent::FocusGained | TerminalEvent::Key(_) | TerminalEvent::Mouse(_) => ime_focused = true,
                            _ => {}
                        }
                        if matches!(event, TerminalEvent::Resize(..)) { media.invalidate(); }
                        for action in app.terminal(event) { controller.action(&mut app, action); }
                    },
                    Some(Err(e))=>return Err(e.into()),None=>app.quit=true,
                },
                Some(result)=media.rx.recv()=>{
                    app.dirty |= media.complete(result);
                },
                Some(event)=native_rx.recv()=>controller.event(&mut app,event),
                Some(work)=work_rx.recv()=>apply_work(&mut app,&mut controller,&store,work),
                _=tick.tick()=>{
                    media.advance(Instant::now());
                    app.expire_notices(Instant::now());
                    if notifier.is_none() && last_notification_claim.elapsed()>Duration::from_secs(5) {
                        last_notification_claim=Instant::now();
                        match crate::notifications::Notifier::start(&paths) {
                            Ok(owner)=>notifier=owner,
                            Err(error)=>{app.notice=format!("桌面通知不可用：{error}");app.dirty=true;}
                        }
                    }
                    if let Some(chat) = app.chat_mut()
                        && chat.read.retry_after.is_some_and(|deadline| deadline <= Instant::now()) {
                        chat.read.retry_after = None;
                        app.dirty = true;
                    }
                    if let Some(peer)=app.selected_peer.clone() && peer.is_group() && app.chat().is_some_and(|c|!c.permission_pending && (c.permission_at.is_none_or(|t|t.elapsed()>Duration::from_secs(30)) || c.permission.as_ref().is_some_and(|p|p.state=="blocked" && p.until.as_ref().and_then(|s|s.parse::<i64>().ok()).is_some_and(|t|t<=chrono::Utc::now().timestamp())))) {controller.permission(&mut app,peer);}
                    if last_cleanup.elapsed()>Duration::from_secs(60){last_cleanup=Instant::now();let pins=app.chats.values().flat_map(|c|c.draft.attachments.iter().map(|a|a.path.clone())).collect();let cache=paths.cache.clone();let store=store.clone();tokio::spawn(async move{let _=store.cleanup_media(cache,pins).await;});}
                    controller.autosave(&mut app);
                    if app.sync_needed&&app.ready&&!app.syncing && (app.sync_urgent || last_snapshot.elapsed()>=Duration::from_millis(250)){last_snapshot=Instant::now();controller.action(&mut app,Action::Snapshot);}
                    if controller.backend.is_none()&&last_connect.elapsed()>Duration::from_secs(2){last_connect=Instant::now();controller.connect(native_tx.clone());}
                    if controller.backend.is_some()&&!app.ready&&last_status.elapsed()>Duration::from_secs(2){last_status=Instant::now();controller.status();}
                },
                result=&mut terminate=>{result?;app.quit=true;},
            }
        }
            Ok(())
    }.await;
    media.clear_graphics();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while app.staging
        || app.pending_account.is_some()
        || app.chats.values().any(|c| c.draft_saving.is_some())
    {
        match tokio::time::timeout_at(deadline, work_rx.recv()).await {
            Ok(Some(work)) if work.staging_completion() => {
                apply_work(&mut app, &mut controller, &store, work)
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    tokio::task::yield_now().await;
    if let Err(error) = controller.flush_drafts(&app).await {
        return Err(error.context("退出时保存草稿失败"));
    }
    let saved = SavedUi {
        account: app.account.clone(),
        selected_peer: app.selected_peer.clone(),
        views: app
            .chats
            .iter()
            .map(|(p, c)| (p.clone(), c.viewport.clone(), c.selected.clone()))
            .collect(),
    };
    store
        .setting("ui-state".into(), Some(serde_json::to_string(&saved)?))
        .await?;
    if initial_sidebar != (app.config.sidebar_width, app.config.sidebar_visible) {
        crate::runtime::save_ui_config(&paths, &app.config)?;
    }
    media.clear_graphics();
    if let Some(ime) = input_method {
        ime.finish().await;
    }
    interface_result
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use crossterm::{
            event::{
                DisableBracketedPaste, DisableFocusChange, DisableMouseCapture,
                PopKeyboardEnhancementFlags,
            },
            execute,
        };
        let _ = execute!(
            std::io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            crossterm::terminal::SetTitle("qqtui")
        );
        ratatui::restore();
    }
}
