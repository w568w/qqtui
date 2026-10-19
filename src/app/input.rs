use super::*;
impl App {
    pub fn terminal(&mut self, event: TerminalEvent) -> Vec<Action> {
        self.dirty = true;
        let interaction = match &event {
            TerminalEvent::Key(key) => key.kind != crossterm::event::KeyEventKind::Release,
            TerminalEvent::Paste(_) => true,
            TerminalEvent::Mouse(mouse) => {
                !matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
            }
            _ => false,
        };
        if interaction && let Some(chat) = self.chat_mut() {
            chat.quote_jump = None;
        }
        match event {
            TerminalEvent::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
                self.terminal_focused = true;
                self.key(key)
            }
            TerminalEvent::Paste(text) => {
                if self.staging {
                    return vec![];
                }
                let clean = safe_text(&text);
                match &mut self.modal {
                    Modal::EmojiPicker { query, index, .. }
                    | Modal::Switcher { query, index }
                    | Modal::Members { query, index, .. }
                    | Modal::Contacts { query, index, .. } => {
                        query.push_str(&clean.replace('\n', " "));
                        *index = 0;
                        return vec![];
                    }
                    Modal::Search { query, index, .. } => {
                        query.push_str(&clean.replace('\n', " "));
                        *index = 0;
                        return vec![Action::Search(query.clone())];
                    }
                    Modal::AttachmentPath { input, .. } => {
                        input.push_str(clean.trim_end_matches(['\r', '\n']));
                        return vec![];
                    }
                    _ => {}
                }
                if self.mode == Mode::Insert
                    && !self.modal.is_open()
                    && self.draft_editable()
                    && let Some(c) = self.chat_mut()
                {
                    let at = c.draft.cursor;
                    c.edit(at, at, &safe_text(&text));
                }
                vec![]
            }
            TerminalEvent::FocusGained => {
                self.terminal_focused = true;
                if let Some(chat) = self.chat_mut() {
                    chat.read.confirmed.clear();
                }
                vec![]
            }
            TerminalEvent::FocusLost => {
                self.terminal_focused = false;
                if let Some(chat) = self.chat_mut() {
                    chat.viewport.follow = false;
                }
                vec![]
            }
            TerminalEvent::Mouse(m) => {
                use crossterm::event::{MouseButton, MouseEventKind};
                use ratatui::layout::Position;
                if let Modal::EmojiPicker {
                    index,
                    items,
                    query,
                    ..
                } = &mut self.modal
                {
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some((_, selected)) = self
                                .hit_regions
                                .emoji_cells
                                .iter()
                                .find(|(rect, _)| rect.contains(Position::new(m.column, m.row)))
                            {
                                *index = *selected;
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            *index = index.saturating_sub(self.hit_regions.emoji_columns.max(1))
                        }
                        MouseEventKind::ScrollDown => {
                            *index = index
                                .saturating_add(self.hit_regions.emoji_columns.max(1))
                                .min(sticker_choices(items, query).len().saturating_sub(1))
                        }
                        _ => {}
                    }
                    return vec![];
                }
                if let Modal::Forward { index, scroll, .. } = &mut self.modal {
                    let point = Position::new(m.column, m.row);
                    if self.forward_area.contains(point) {
                        match m.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                let row = *scroll + usize::from(m.row - self.forward_area.y);
                                if let Some(i) = self
                                    .forward_rows
                                    .iter()
                                    .position(|(a, b)| *a <= row && row < *b)
                                {
                                    *index = i;
                                }
                            }
                            MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                            MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3),
                            _ => {}
                        }
                    }
                    return vec![];
                }
                if self.modal.is_open() || self.staging || self.pending_account.is_some() {
                    return vec![];
                }
                let point = Position::new(m.column, m.row);
                let conversation = self
                    .hit_regions
                    .conversations
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                    .map(|(_, peer)| peer.clone());
                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    self.terminal_focused = true;
                    self.mode = Mode::Normal;
                    if let Some(peer) = conversation {
                        self.focus = Focus::Conversations;
                        self.sidebar_cursor = Some(peer.clone());
                        if peer.is_folder() {
                            return self.activate_conversation(peer);
                        }
                        return vec![Action::Select(peer)];
                    }
                    if self.hit_regions.composer.contains(point) {
                        self.focus = Focus::Messages;
                        if self.draft_editable() {
                            self.mode = Mode::Insert;
                        }
                    } else if self.hit_regions.message_area.contains(point) {
                        self.focus = Focus::Messages;
                        let selected = self
                            .hit_regions
                            .messages
                            .iter()
                            .find(|(rect, _)| rect.contains(point))
                            .map(|(_, id)| id.clone());
                        if let Some(chat) = self.chat_mut() {
                            chat.selected = selected;
                        }
                    }
                } else if matches!(
                    m.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let delta = if m.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    };
                    if self.hit_regions.sidebar.contains(point) {
                        self.move_conversation_cursor(delta);
                    } else if self.hit_regions.message_area.contains(point)
                        && let Some(chat) = self.chat_mut()
                    {
                        chat.scroll_lines(delta);
                        return chat
                            .history_edge(delta)
                            .into_iter()
                            .map(Action::History)
                            .collect();
                    }
                }
                vec![]
            }
            _ => vec![],
        }
    }
    pub(crate) fn move_conversation_cursor(&mut self, delta: isize) {
        if self.conversations.is_empty() {
            return;
        }
        let index = self
            .sidebar_cursor
            .as_ref()
            .or(self.selected_peer.as_ref())
            .and_then(|p| self.conversations.iter().position(|c| &c.peer == p))
            .unwrap_or(0);
        self.sidebar_cursor = Some(
            self.conversations[index
                .saturating_add_signed(delta)
                .min(self.conversations.len() - 1)]
            .peer
            .clone(),
        );
    }
    pub(crate) fn activate_conversation(&mut self, peer: Peer) -> Vec<Action> {
        if let Some(c) = find_conversation(&self.conversations, &peer) {
            if !c.children.is_empty() {
                self.modal = Modal::Contacts {
                    query: String::new(),
                    index: 0,
                    items: c.children.clone(),
                };
            } else if peer.is_folder() {
                return vec![Action::Select(peer), Action::Folder];
            } else {
                self.focus = Focus::Messages;
                if self.selected_peer.as_ref() != Some(&peer) {
                    return vec![Action::Select(peer)];
                }
            }
        }
        vec![]
    }
    pub(crate) fn key(&mut self, mut key: KeyEvent) -> Vec<Action> {
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && let KeyCode::Char(c) = key.code
            && c.is_ascii_lowercase()
        {
            key.code = KeyCode::Char(c.to_ascii_uppercase());
        }
        if (self.staging || self.pending_account.is_some())
            && !matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
        {
            self.notice = "正在提交，请稍候".into();
            return vec![];
        }
        if self.modal.is_open() {
            return self.modal_key(key);
        }
        if key.code == KeyCode::Char('V') && self.binding("preview") == "v" {
            key.code = KeyCode::Char('v');
            key.modifiers.remove(KeyModifiers::SHIFT);
        }
        let name = key_name(key);
        if self.staging
            && [
                self.binding("paste"),
                self.binding("attach"),
                self.binding("reply"),
            ]
            .contains(&name)
        {
            return vec![];
        }
        if key.code == KeyCode::Enter && self.mode == Mode::Normal {
            if self.focus == Focus::Messages
                && let Some(message) = self.chat().and_then(|c| c.selected_message())
                && message.forward.is_some()
            {
                return vec![Action::Forward {
                    root: message.clone(),
                    path: vec![message.id.clone()],
                }];
            }
            let target = if self.focus == Focus::Conversations {
                self.sidebar_cursor.as_ref().or(self.selected_peer.as_ref())
            } else {
                self.selected_peer.as_ref()
            }
            .cloned();
            if let Some(peer) = target {
                return self.activate_conversation(peer);
            }
            return vec![];
        }
        if name == self.binding("switch") {
            self.modal = Modal::Switcher {
                query: String::new(),
                index: 0,
            };
            return vec![];
        }
        if name == self.binding("paste") {
            return vec![Action::Paste];
        }
        if name == self.binding("stickers") {
            if self.draft_editable() {
                return vec![Action::EmojiCatalog(EmojiPurpose::Send)];
            }
            return vec![];
        }
        if self.mode == Mode::Insert {
            if key.code == KeyCode::Esc {
                self.mode = Mode::Normal;
                return vec![];
            }
            if !self.draft_editable() {
                return vec![];
            }
            if name == self.binding("send") {
                return vec![Action::Send];
            }
            if name == self.binding("newline") {
                if !self.staging
                    && let Some(c) = self.chat_mut()
                {
                    c.edit(c.draft.cursor, c.draft.cursor, "\n");
                }
                return vec![];
            }
            if self.staging {
                return vec![];
            }
            let group_input = self.selected_peer.as_ref().is_some_and(|p| p.is_group());
            if let Some(c) = self.chat_mut() {
                let pos = c.draft.cursor.min(c.draft.text.len());
                let version = c.draft_version;
                match key.code {
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        c.edit(pos, pos, &ch.to_string());
                        if ch == '@' && group_input {
                            return vec![Action::Members(true)];
                        }
                    }
                    KeyCode::Backspace => {
                        let before = previous_boundary(&c.draft.text, pos);
                        c.edit(before, pos, "");
                    }
                    KeyCode::Delete => {
                        let next = next_boundary(&c.draft.text, pos);
                        c.edit(pos, next, "");
                    }
                    KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        c.draft.cursor = word_boundary(&c.draft.text, pos, false)
                    }
                    KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        c.draft.cursor = word_boundary(&c.draft.text, pos, true)
                    }
                    KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        c.draft.cursor = 0
                    }
                    KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        c.draft.cursor = c.draft.text.len()
                    }
                    KeyCode::Left => c.draft.cursor = previous_boundary(&c.draft.text, pos),
                    KeyCode::Right => c.draft.cursor = next_boundary(&c.draft.text, pos),
                    KeyCode::Home => {
                        c.draft.cursor = c.draft.text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
                    }
                    KeyCode::End => {
                        c.draft.cursor = c.draft.text[pos..]
                            .find('\n')
                            .map(|i| pos + i)
                            .unwrap_or(c.draft.text.len())
                    }
                    KeyCode::Up => move_cursor_vertical(c, -1),
                    KeyCode::Down => move_cursor_vertical(c, 1),
                    _ => {}
                }
                if c.draft.cursor != pos && c.draft_version == version {
                    c.changed();
                }
            }
            return vec![];
        }
        if key.code == KeyCode::Esc {
            self.key_prefix.clear();
            return vec![];
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            self.focus = if self.focus == Focus::Conversations {
                Focus::Messages
            } else {
                Focus::Conversations
            };
            return vec![];
        }
        if self.prefix_at.elapsed() > Duration::from_secs(1) {
            self.key_prefix.clear();
        }
        self.key_prefix.push_str(&name);
        self.prefix_at = Instant::now();
        let actions = [
            "left",
            "right",
            "next",
            "next_unread",
            "previous_unread",
            "previous",
            "top",
            "bottom",
            "page_up",
            "page_down",
            "reply",
            "copy",
            "insert",
            "search",
            "help",
            "quit",
            "actions",
            "members",
            "contacts",
            "attach",
            "sticker_file",
            "clear",
            "sidebar",
            "narrower",
            "wider",
            "outbox",
            "profile",
            "preview",
            "open",
            "quote",
            "first_unread",
            "back",
            "reactions",
        ];
        let action = actions
            .into_iter()
            .find(|a| self.binding(a) == self.key_prefix);
        if action.is_none() {
            if !actions
                .into_iter()
                .any(|a| self.binding(a).starts_with(&self.key_prefix))
            {
                self.key_prefix.clear();
            }
            return vec![];
        }
        self.key_prefix.clear();
        match action.unwrap() {
            direction @ ("next_unread" | "previous_unread") => {
                let items = self.switcher_items("");
                fn excluded(items: &[Conversation], parent: bool, peers: &mut HashSet<Peer>) {
                    for c in items {
                        let skip = parent || c.peer.is_folder();
                        if skip {
                            peers.insert(c.peer.clone());
                        }
                        excluded(&c.children, skip, peers);
                    }
                }
                let mut skipped = HashSet::new();
                excluded(&self.conversations, false, &mut skipped);
                let current = items
                    .iter()
                    .position(|c| Some(&c.peer) == self.selected_peer.as_ref());
                let next = (1..=items.len()).find_map(|offset| {
                    let index = if direction == "next_unread" {
                        (current.unwrap_or(items.len() - 1) + offset) % items.len()
                    } else {
                        (current.unwrap_or(0) + items.len() - offset) % items.len()
                    };
                    let c = items[index];
                    (!skipped.contains(&c.peer)
                        && chat_supported(&c.peer)
                        && c.unread.parse::<u64>().is_ok_and(|n| n > 0))
                    .then(|| c.peer.clone())
                });
                if let Some(peer) = next {
                    self.focus = Focus::Messages;
                    return vec![Action::Select(peer)];
                }
                self.notice = "没有符合条件的未读会话".into();
            }
            "first_unread" => return vec![Action::FirstUnread],
            "reactions" => {
                if let Some(message) = self.chat().and_then(|c| c.selected_message()) {
                    return vec![Action::Reactions(message.clone())];
                }
            }
            "clear" => {
                if self.staging {
                    self.notice = "正在提交消息，请等待提交结果".into();
                } else if self.draft_editable() {
                    let chat = self.chat_mut().unwrap();
                    chat.draft = Draft::default();
                    chat.composer_generation = chat.composer_generation.wrapping_add(1);
                    chat.changed();
                }
            }
            "quit" => return vec![Action::Quit],
            "left" => self.focus = Focus::Conversations,
            "right" => self.focus = Focus::Messages,
            "next" | "previous" | "page_up" | "page_down" => {
                let delta = match action.unwrap() {
                    "next" => 1,
                    "previous" => -1,
                    "page_up" => {
                        -(self.chat().map(|c| c.view_height / 2).unwrap_or(10).max(1) as isize)
                    }
                    _ => self.chat().map(|c| c.view_height / 2).unwrap_or(10).max(1) as isize,
                };
                if self.focus == Focus::Conversations {
                    self.move_conversation_cursor(delta);
                } else if let Some(c) = self.chat_mut() {
                    if matches!(action, Some("page_up" | "page_down")) {
                        c.scroll_lines(delta);
                    } else {
                        c.move_selection(delta);
                    }
                    if let Some(position) = c.history_edge(delta) {
                        return vec![Action::History(position)];
                    }
                }
            }
            "top" => {
                if self.focus == Focus::Conversations {
                    if let Some(c) = self.conversations.first() {
                        self.sidebar_cursor = Some(c.peer.clone());
                    }
                } else if let Some(c) = self.chat_mut() {
                    c.move_selection(-(c.messages.len() as isize));
                    if !c.loading
                        && !c.exhausted
                        && let Some(first) = c.messages.first()
                    {
                        return vec![Action::History(HistoryPoint::Before(first.id.clone()))];
                    }
                }
            }
            "bottom" => {
                if self.focus == Focus::Conversations {
                    if let Some(c) = self.conversations.last() {
                        self.sidebar_cursor = Some(c.peer.clone());
                    }
                } else if let Some(c) = self.chat_mut() {
                    c.viewport.follow = true;
                    c.new_messages = 0;
                    c.selected = c.messages.last().map(|m| m.id.clone());
                    if c.has_later || !c.loaded {
                        return vec![Action::History(HistoryPoint::Latest)];
                    }
                }
            }
            "insert" => {
                if self
                    .selected_peer
                    .as_ref()
                    .is_none_or(|p| !chat_supported(p))
                {
                    self.notice = "此入口不是聊天会话，按 Enter 展开".into();
                    return vec![];
                }
                if !self.draft_editable() {
                    return vec![];
                }
                self.mode = Mode::Insert;
                self.focus = Focus::Messages;
            }
            "reply" => {
                if self
                    .chat()
                    .and_then(|c| c.selected_message())
                    .is_some_and(|m| m.recalled)
                {
                    self.notice = "该消息已撤回".into();
                    return vec![];
                }
                if self.selected_peer.as_ref().is_some_and(|p| p.is_device()) {
                    self.notice = "QQ 设备会话不支持引用回复".into();
                    return vec![];
                }
                if !self.draft_editable() {
                    return vec![];
                }
                if let Some(c) = self.chat_mut()
                    && let Some(m) = c.selected_message()
                {
                    c.draft.reply_to = Some(m.id.clone());
                    c.changed();
                    self.mode = Mode::Insert;
                    self.focus = Focus::Messages;
                }
            }
            "copy" => {
                if let Some(m) = self.chat().and_then(|c| c.selected_message()) {
                    return vec![Action::Copy(m.text.clone())];
                }
            }
            "help" => self.modal = Modal::Help,
            "actions" => {
                if self.focus == Focus::Conversations {
                    if let Some(c) = self
                        .sidebar_cursor
                        .as_ref()
                        .or(self.selected_peer.as_ref())
                        .and_then(|p| find_conversation(&self.conversations, p))
                    {
                        self.modal = Modal::ConversationActions {
                            conversation: c.clone(),
                            index: 0,
                        };
                    }
                } else {
                    self.modal = Modal::Actions { index: 0 };
                }
            }
            "search" => {
                self.modal = Modal::Search {
                    query: String::new(),
                    index: 0,
                    results: vec![],
                }
            }
            "members" => return vec![Action::Members(false)],
            "profile" => {
                if self.selected_peer.as_ref().is_some_and(|p| p.is_group()) {
                    return vec![Action::Members(false)];
                }
                if let Some(peer) = &self.selected_peer
                    && peer.chat_type != 8
                {
                    return vec![Action::Profile(peer.uid.clone())];
                }
            }
            "contacts" => return vec![Action::Contacts],
            "preview" | "open" => {
                if let Some(message) = self.chat().and_then(|c| c.selected_message()).cloned() {
                    if message.forward.is_some() {
                        return vec![Action::Forward {
                            path: vec![message.id.clone()],
                            root: message,
                        }];
                    }
                    if action == Some("preview") {
                        if let Some(attachment) = message
                            .attachments
                            .iter()
                            .find(|a| a.is_image_like())
                            .cloned()
                        {
                            self.modal = Modal::Preview {
                                message,
                                attachment,
                                zoom: PreviewZoom::Normal,
                            };
                        } else {
                            self.notice = "此消息没有图片".into();
                        }
                    } else {
                        let items = message_objects(&message);
                        if items.is_empty() {
                            self.notice = "此消息没有可打开的附件或链接".into();
                        } else {
                            let (modal, actions) = open_objects(message, items);
                            self.modal = modal;
                            return actions;
                        }
                    }
                }
            }
            "quote" => {
                if let Some(message) = self
                    .chat()
                    .and_then(|c| c.selected_message())
                    .filter(|m| m.quote_key().is_some())
                    .cloned()
                {
                    return vec![Action::JumpQuote(message)];
                }
                self.notice = "此消息没有引用".into();
            }
            "back" => return vec![Action::JumpBack],
            "attach" | "sticker_file" => {
                if !self.draft_editable() {
                    return vec![];
                }
                self.modal = Modal::AttachmentPath {
                    input: String::new(),
                    sticker: action == Some("sticker_file"),
                }
            }
            "outbox" => {
                let mut items: Vec<_> = self
                    .receipts
                    .values()
                    .filter(|(r, _)| r.state != "confirmed")
                    .cloned()
                    .collect();
                items.sort_by(|a, b| a.0.request_id.cmp(&b.0.request_id));
                self.modal = Modal::Outbox { index: 0, items };
            }
            "sidebar" => self.config.sidebar_visible = !self.config.sidebar_visible,
            "narrower" => {
                self.config.sidebar_width = self.config.sidebar_width.saturating_sub(2).max(16)
            }
            "wider" => self.config.sidebar_width = (self.config.sidebar_width + 2).min(60),
            _ => {}
        }
        vec![]
    }
    pub(crate) fn modal_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.code == KeyCode::Esc
            && let Modal::ReactionMembers { message, .. } = &self.modal
        {
            return vec![Action::Reactions(message.clone())];
        }
        if key.code == KeyCode::Esc
            && matches!(self.modal, Modal::Forward { .. })
            && let Some(parent) = self.forward_parents.pop()
        {
            self.modal = parent;
            return vec![];
        }
        if key.code == KeyCode::Esc
            && let Modal::Forward { root, path, .. } = &self.modal
            && path.len() > 1
        {
            return vec![Action::Forward {
                root: root.clone(),
                path: path[..path.len() - 1].to_vec(),
            }];
        }
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('q')
                && matches!(
                    self.modal,
                    Modal::Help
                        | Modal::Forward { .. }
                        | Modal::Actions { .. }
                        | Modal::ConversationActions { .. }
                        | Modal::Attachments { .. }
                        | Modal::Objects { .. }
                        | Modal::ConfirmRecall(_)
                        | Modal::Profile(_)
                )
        {
            self.pending_dialog = None;
            if matches!(self.modal, Modal::Loading(_)) {
                self.notice = "已取消读取".into();
            }
            self.modal = self
                .modal_return
                .take()
                .map(|modal| *modal)
                .unwrap_or(Modal::None);
            return vec![];
        }
        let mut actions = vec![];
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        self.modal = match modal {
            Modal::EmojiPicker {
                token,
                purpose,
                items,
                mut query,
                mut index,
            } => {
                let choices = sticker_choices(&items, &query);
                if key.code == KeyCode::Char('f')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(purpose, EmojiPurpose::Send)
                {
                    Modal::AttachmentPath {
                        input: String::new(),
                        sticker: true,
                    }
                } else if key.code == KeyCode::Enter {
                    if let Some(sticker) = choices.get(index).cloned().cloned() {
                        match purpose {
                            EmojiPurpose::Send => actions.push(Action::ChooseSticker(sticker)),
                            EmojiPurpose::React(message) => {
                                let existing = message.reactions.iter().find(|r| {
                                    r.emoji_id == sticker.emoji_id
                                        && r.emoji_type == sticker.emoji_type
                                });
                                let set = existing.is_none_or(|r| !r.clicked);
                                actions.push(Action::SetReaction {
                                    message: *message,
                                    reaction: Reaction {
                                        emoji_id: sticker.emoji_id,
                                        emoji_type: sticker.emoji_type,
                                        label: sticker.name,
                                        ..Default::default()
                                    },
                                    set,
                                });
                            }
                        }
                    }
                    Modal::None
                } else {
                    let delta = match key.code {
                        KeyCode::Left | KeyCode::BackTab => -1,
                        KeyCode::Right | KeyCode::Tab => 1,
                        KeyCode::Up => -(self.hit_regions.emoji_columns.max(1) as isize),
                        KeyCode::Down => self.hit_regions.emoji_columns.max(1) as isize,
                        KeyCode::PageUp => -(self.hit_regions.emoji_cells.len() as isize),
                        KeyCode::PageDown => self.hit_regions.emoji_cells.len() as isize,
                        KeyCode::Home => -(index as isize),
                        KeyCode::End => choices.len().saturating_sub(1) as isize - index as isize,
                        _ => 0,
                    };
                    if delta != 0 {
                        index = index
                            .saturating_add_signed(delta)
                            .min(choices.len().saturating_sub(1));
                    } else {
                        match key.code {
                            KeyCode::Char(c)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                query.push(c);
                                index = 0;
                            }
                            KeyCode::Backspace => {
                                query.truncate(previous_boundary(&query, query.len()));
                                index = 0;
                            }
                            _ => {}
                        }
                    }
                    Modal::EmojiPicker {
                        token,
                        purpose,
                        items,
                        query,
                        index,
                    }
                }
            }
            Modal::Reactions { message, mut index } => {
                if key.code == KeyCode::Char('a') {
                    actions.push(Action::EmojiCatalog(EmojiPurpose::React(Box::new(message))));
                    Modal::None
                } else if let Some(reaction) = message.reactions.get(index).cloned() {
                    if key.code == KeyCode::Enter {
                        actions.push(Action::ReactionMembers {
                            message,
                            reaction,
                            cookie: None,
                        });
                        Modal::None
                    } else if key.code == KeyCode::Char(' ') {
                        let set = !reaction.clicked;
                        actions.push(Action::SetReaction {
                            message,
                            reaction,
                            set,
                        });
                        Modal::None
                    } else {
                        move_picker(key, &mut index, message.reactions.len());
                        Modal::Reactions { message, index }
                    }
                } else {
                    Modal::Reactions { message, index }
                }
            }
            Modal::ReactionMembers {
                message,
                reaction,
                page,
                mut index,
            } => {
                if key.code == KeyCode::Char('n') && page.cookie.is_some() {
                    actions.push(Action::ReactionMembers {
                        message,
                        reaction,
                        cookie: page.cookie,
                    });
                    Modal::None
                } else {
                    move_picker(key, &mut index, page.members.len());
                    Modal::ReactionMembers {
                        message,
                        reaction,
                        page,
                        index,
                    }
                }
            }
            Modal::Forward {
                root,
                mut path,
                messages,
                mut index,
                mut scroll,
            } => {
                if (key_name(key) == self.binding("open")
                    || matches!(key.code, KeyCode::Char('v' | 'V')))
                    && let Some(message) = messages.get(index).cloned()
                {
                    let parent = Modal::Forward {
                        root: root.clone(),
                        path: path.clone(),
                        messages: messages.clone(),
                        index,
                        scroll,
                    };
                    if key_name(key) == self.binding("open") {
                        let items = message_objects(&message);
                        let (modal, pending) = open_objects(message, items);
                        actions.extend(pending);
                        if modal.is_open() {
                            self.modal_return = Some(Box::new(parent));
                            modal
                        } else {
                            parent
                        }
                    } else if let Some(attachment) = message
                        .attachments
                        .iter()
                        .find(|a| a.is_image_like())
                        .cloned()
                    {
                        self.modal_return = Some(Box::new(parent));
                        Modal::Preview {
                            message,
                            attachment,
                            zoom: PreviewZoom::Normal,
                        }
                    } else {
                        parent
                    }
                } else if key.code == KeyCode::Enter
                    && messages.get(index).is_some_and(|m| m.forward.is_some())
                {
                    if path.len() >= 8 {
                        self.notice = "已达到八层转发上限".into();
                        self.modal = Modal::Forward {
                            root,
                            path,
                            messages,
                            index,
                            scroll,
                        };
                        return actions;
                    }
                    self.forward_parents.push(Modal::Forward {
                        root: root.clone(),
                        path: path.clone(),
                        messages: messages.clone(),
                        index,
                        scroll,
                    });
                    path.push(messages[index].id.clone());
                    actions.push(Action::Forward { root, path });
                    Modal::None
                } else {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            index = (index + 1).min(messages.len().saturating_sub(1));
                            self.forward_reveal = true;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            index = index.saturating_sub(1);
                            self.forward_reveal = true;
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            scroll = scroll
                                .saturating_add(usize::from(self.forward_area.height / 2).max(1))
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            scroll = scroll
                                .saturating_sub(usize::from(self.forward_area.height / 2).max(1))
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            index = 0;
                            self.forward_reveal = true;
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            index = messages.len().saturating_sub(1);
                            self.forward_reveal = true;
                        }
                        _ => {}
                    }
                    Modal::Forward {
                        root,
                        path,
                        messages,
                        index,
                        scroll,
                    }
                }
            }
            Modal::Switcher {
                mut query,
                mut index,
            } => {
                let items = self.switcher_items(&query);
                let len = items.len();
                if key.code == KeyCode::Enter {
                    if let Some(c) = items.get(index.min(len.saturating_sub(1))) {
                        actions.push(Action::Select(c.peer.clone()));
                        self.focus = Focus::Messages;
                    }
                    Modal::None
                } else {
                    edit_picker(key, &mut query, &mut index, len);
                    Modal::Switcher { query, index }
                }
            }
            Modal::Search {
                mut query,
                mut index,
                results,
            } => {
                if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
                    actions.push(Action::Search(query.clone()));
                    Modal::Search {
                        query,
                        index: 0,
                        results,
                    }
                } else if key.code == KeyCode::Enter && !results.is_empty() {
                    if let Some(m) = results.get(index)
                        && let Some(c) = self.chat_mut()
                    {
                        c.viewport.follow = false;
                        c.selected = Some(m.id.clone());
                        c.viewport.anchor = Some(m.id.clone());
                        c.viewport.line = 0;
                        c.merge(vec![m.clone()]);
                        actions.push(Action::History(HistoryPoint::Around(m.id.clone())));
                    }
                    Modal::None
                } else {
                    let old = query.clone();
                    edit_picker(key, &mut query, &mut index, results.len());
                    if query != old {
                        actions.push(Action::Search(query.clone()));
                    }
                    Modal::Search {
                        query,
                        index,
                        results,
                    }
                }
            }
            Modal::AttachmentPath { mut input, sticker } => {
                if key.code == KeyCode::Enter {
                    actions.push(Action::Attach {
                        file: input,
                        sticker,
                    });
                    Modal::None
                } else {
                    let mut i = 0;
                    edit_picker(key, &mut input, &mut i, 0);
                    Modal::AttachmentPath { input, sticker }
                }
            }
            Modal::ConversationActions {
                conversation,
                mut index,
            } => {
                if key.code == KeyCode::Enter && index == 0 {
                    actions.push(Action::MarkRead(conversation));
                    self.modal_return.take().map(|m| *m).unwrap_or(Modal::None)
                } else {
                    index = 0;
                    Modal::ConversationActions {
                        conversation,
                        index,
                    }
                }
            }
            Modal::Actions { mut index } => {
                let labels = action_labels(self);
                if key.code == KeyCode::Enter {
                    let enabled = labels.get(index).is_some_and(|(_, enabled)| *enabled);
                    if !enabled {
                        Modal::Actions { index }
                    } else {
                        let message = self.chat().and_then(|c| c.selected_message()).cloned();
                        match index {
                            0 => {
                                if let Some(m) = message {
                                    actions.push(Action::Copy(m.text));
                                }
                                Modal::None
                            }
                            1 => {
                                if let Some(c) = self.chat_mut() {
                                    c.draft.reply_to = message.map(|m| m.id);
                                    c.changed();
                                    self.mode = Mode::Insert;
                                }
                                Modal::None
                            }
                            2 => message.map(Modal::ConfirmRecall).unwrap_or(Modal::None),
                            3 => message
                                .map(|message| Modal::Attachments { message, index: 0 })
                                .unwrap_or(Modal::None),
                            4 => {
                                if let Some(m) = message {
                                    let items = message_urls(&m.text)
                                        .into_iter()
                                        .map(OpenTarget::Url)
                                        .collect();
                                    let (modal, pending) = open_objects(m, items);
                                    actions.extend(pending);
                                    modal
                                } else {
                                    Modal::None
                                }
                            }
                            5 => {
                                if let Some(c) = self.chat_mut() {
                                    c.draft.reply_to = None;
                                    c.changed();
                                }
                                Modal::None
                            }
                            6 => {
                                if let Some(c) = self.chat_mut() {
                                    c.draft.attachments.pop();
                                    c.changed();
                                }
                                Modal::None
                            }
                            7 => {
                                if let Some(message) = message {
                                    actions.push(Action::JumpQuote(message));
                                }
                                Modal::None
                            }
                            8 => {
                                if let Some(message) = message {
                                    actions.push(Action::Reactions(message));
                                }
                                Modal::None
                            }
                            9 => {
                                actions.push(Action::EmojiCatalog(EmojiPurpose::Send));
                                Modal::None
                            }
                            _ => Modal::None,
                        }
                    }
                } else {
                    move_picker(key, &mut index, labels.len());
                    Modal::Actions { index }
                }
            }
            Modal::ConfirmRecall(m) => {
                if key.code == KeyCode::Enter {
                    actions.push(Action::Recall(m));
                    Modal::None
                } else {
                    Modal::ConfirmRecall(m)
                }
            }
            Modal::Preview {
                message,
                mut attachment,
                mut zoom,
            } => {
                if key_name(key) == self.binding("open") {
                    actions.push(Action::Resolve(message.clone(), attachment.clone()));
                } else {
                    match key.code {
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            zoom = match zoom {
                                PreviewZoom::Normal => PreviewZoom::Large,
                                _ => PreviewZoom::Fullscreen,
                            }
                        }
                        KeyCode::Char('-') => {
                            zoom = match zoom {
                                PreviewZoom::Fullscreen => PreviewZoom::Large,
                                _ => PreviewZoom::Normal,
                            }
                        }
                        KeyCode::Char('z') => {
                            zoom = if zoom == PreviewZoom::Fullscreen {
                                PreviewZoom::Normal
                            } else {
                                PreviewZoom::Fullscreen
                            }
                        }
                        KeyCode::Char('0') => zoom = PreviewZoom::Normal,
                        KeyCode::Char('r') => {
                            let key = message.media_key(&attachment.element_id);
                            self.failed_images.remove(&key);
                            attachment.path.clear();
                            attachment.warning = None;
                        }
                        KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'j' | 'k' | 'l') => {
                            let images: Vec<_> = message.image_attachments().collect();
                            if let Some(current) = images
                                .iter()
                                .position(|a| a.element_id == attachment.element_id)
                            {
                                let previous =
                                    matches!(key.code, KeyCode::Left | KeyCode::Char('h' | 'k'));
                                let next = if previous {
                                    current.saturating_sub(1)
                                } else {
                                    (current + 1).min(images.len() - 1)
                                };
                                attachment = images[next].clone();
                            }
                        }
                        _ => {}
                    }
                }
                Modal::Preview {
                    message,
                    attachment,
                    zoom,
                }
            }
            Modal::Objects {
                message,
                items,
                mut index,
            } => {
                let shortcut = match key.code {
                    KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                        Some((ch as usize) - ('1' as usize))
                    }
                    _ => None,
                };
                if key.code == KeyCode::Enter || shortcut.is_some() {
                    if let Some(item) = items.get(shortcut.unwrap_or(index)) {
                        actions.push(item.action(&message));
                        self.modal_return
                            .take()
                            .map(|modal| *modal)
                            .unwrap_or(Modal::None)
                    } else {
                        Modal::Objects {
                            message,
                            items,
                            index,
                        }
                    }
                } else {
                    move_picker(key, &mut index, items.len());
                    Modal::Objects {
                        message,
                        items,
                        index,
                    }
                }
            }
            Modal::Attachments { message, mut index } => {
                if key.code == KeyCode::Enter {
                    if let Some(a) = message.attachments.get(index) {
                        actions.push(Action::Resolve(message.clone(), a.clone()));
                    }
                    Modal::None
                } else {
                    move_picker(key, &mut index, message.attachments.len());
                    Modal::Attachments { message, index }
                }
            }
            Modal::Members {
                mut query,
                mut index,
                members,
                mention,
            } => {
                let filtered: Vec<_> = members
                    .iter()
                    .filter(|m| {
                        m.name.to_lowercase().contains(&query.to_lowercase())
                            || m.uin.contains(&query)
                    })
                    .collect();
                if key.code == KeyCode::Enter && !mention {
                    if let Some(member) = filtered.get(index) {
                        actions.push(Action::Profile(member.uid.clone()));
                    }
                    Modal::None
                } else if key.code == KeyCode::Enter && mention {
                    if let Some(m) = filtered.get(index)
                        && let Some(c) = self.chat_mut()
                    {
                        let end = c.draft.cursor;
                        let start = if c.draft.text[..end].ends_with('@') {
                            end - 1
                        } else {
                            end
                        };
                        let text = format!("@{} ", m.name);
                        c.edit(start, end, &text);
                        c.draft.mentions.push(Mention {
                            start,
                            end: start + text.len() - 1,
                            uid: m.uid.clone(),
                            uin: m.uin.clone(),
                        });
                    }
                    Modal::None
                } else {
                    edit_picker(key, &mut query, &mut index, filtered.len());
                    Modal::Members {
                        query,
                        index,
                        members,
                        mention,
                    }
                }
            }
            Modal::Contacts {
                mut query,
                mut index,
                items,
            } => {
                let filtered: Vec<_> = items
                    .iter()
                    .filter(|c| c.name.to_lowercase().contains(&query.to_lowercase()))
                    .collect();
                if key_name(key) == self.binding("actions") {
                    if let Some(conversation) = filtered.get(index).cloned().cloned() {
                        self.modal_return = Some(Box::new(Modal::Contacts {
                            query,
                            index,
                            items,
                        }));
                        Modal::ConversationActions {
                            conversation,
                            index: 0,
                        }
                    } else {
                        Modal::Contacts {
                            query,
                            index,
                            items,
                        }
                    }
                } else if key.code == KeyCode::Enter {
                    if let Some(c) = filtered.get(index) {
                        actions.push(Action::Select(c.peer.clone()));
                        self.focus = Focus::Messages;
                    }
                    Modal::None
                } else {
                    edit_picker(key, &mut query, &mut index, filtered.len());
                    Modal::Contacts {
                        query,
                        index,
                        items,
                    }
                }
            }
            Modal::Outbox { mut index, items } => {
                if key.code == KeyCode::Enter {
                    if let Some((receipt, draft)) = items.get(index) {
                        actions.push(Action::RestoreDraft(receipt.clone(), draft.clone()));
                    }
                    Modal::None
                } else {
                    move_picker(key, &mut index, items.len());
                    Modal::Outbox { index, items }
                }
            }
            m => m,
        };
        if !self.modal.is_open() {
            self.pending_dialog = None;
        }
        actions
    }
}
fn key_name(key: KeyEvent) -> String {
    let base = match key.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        _ => format!("{:?}", key.code).to_lowercase(),
    };
    let mut s = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        s.push_str("alt-");
    }
    s + &base
}
fn move_cursor_vertical(c: &mut Chat, delta: isize) {
    let pos = c.draft.cursor;
    let start = c.draft.text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = c.draft.text[start..pos].graphemes(true).count();
    let target = if delta < 0 {
        if start == 0 {
            return;
        }
        let end = start - 1;
        let begin = c.draft.text[..end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        (begin, end)
    } else {
        let Some(end) = c.draft.text[pos..].find('\n').map(|i| i + pos) else {
            return;
        };
        let begin = end + 1;
        let end = c.draft.text[begin..]
            .find('\n')
            .map(|i| begin + i)
            .unwrap_or(c.draft.text.len());
        (begin, end)
    };
    c.draft.cursor = c.draft.text[target.0..target.1]
        .grapheme_indices(true)
        .nth(col)
        .map(|(i, _)| target.0 + i)
        .unwrap_or(target.1);
}
fn move_picker(key: KeyEvent, index: &mut usize, len: usize) {
    let delta = match key.code {
        KeyCode::Down | KeyCode::Char('j') => 1,
        KeyCode::Up | KeyCode::Char('k') => -1,
        _ => 0,
    };
    *index = index
        .saturating_add_signed(delta)
        .min(len.saturating_sub(1));
}
fn edit_picker(key: KeyEvent, text: &mut String, index: &mut usize, len: usize) {
    move_picker(key, index, len);
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            text.push(c);
            *index = 0;
        }
        KeyCode::Backspace => {
            text.truncate(previous_boundary(text, text.len()));
            *index = 0;
        }
        _ => {}
    }
}
pub fn action_labels(app: &App) -> Vec<(String, bool)> {
    let message = app.chat().and_then(|c| c.selected_message());
    let selected = message.is_some();
    let no_message = (!selected).then_some("未选择消息");
    let chat_reason = no_message
        .or_else(|| message.is_some_and(|m| m.recalled).then_some("消息已撤回"))
        .or_else(|| {
            app.selected_peer
                .as_ref()
                .is_some_and(|p| p.is_device())
                .then_some("设备会话不支持")
        })
        .or_else(|| {
            app.selected_peer
                .as_ref()
                .is_some_and(|p| p.is_mailbox())
                .then_some("QQ 邮箱提醒仅支持查看")
        })
        .or_else(|| (!app.ready).then_some("QQ 尚未就绪"));
    let label = |text: &str, reason: Option<&str>| match reason {
        Some(reason) => (format!("{text}（{reason}）"), false),
        None => (text.into(), true),
    };
    vec![
        label("复制正文", no_message),
        label(
            "回复",
            chat_reason.or_else(|| {
                app.chat()
                    .is_none_or(|c| !c.draft_loaded)
                    .then_some("草稿尚未读取")
            }),
        ),
        label(
            "撤回",
            chat_reason.or_else(|| {
                (message.is_some_and(|m| !m.is_self)
                    && (app.selected_peer.as_ref().is_none_or(|p| !p.is_group())
                        || app
                            .chat()
                            .and_then(|c| c.permission.as_ref())
                            .is_none_or(|p| !p.can_moderate)))
                .then_some("需要群管理权限")
            }),
        ),
        label(
            "查看附件",
            no_message.or_else(|| {
                message
                    .is_some_and(|m| m.attachments.is_empty())
                    .then_some("此消息无附件")
            }),
        ),
        label(
            "打开链接",
            no_message.or_else(|| {
                message
                    .and_then(|m| first_url(&m.text))
                    .is_none()
                    .then_some("此消息无链接")
            }),
        ),
        label(
            "取消回复",
            app.chat()
                .is_none_or(|c| c.draft.reply_to.is_none())
                .then_some("未选择回复对象"),
        ),
        label(
            "移除最后一个待发附件",
            app.chat()
                .is_none_or(|c| c.draft.attachments.is_empty())
                .then_some("没有待发附件"),
        ),
        label(
            "跳转到引用原消息",
            no_message.or_else(|| {
                message
                    .and_then(Message::quote_key)
                    .is_none()
                    .then_some("此消息没有引用")
            }),
        ),
        label(
            "表情回应（查看／添加）",
            chat_reason.or_else(|| {
                message
                    .is_none_or(|m| !m.peer.is_group())
                    .then_some("仅群聊可用")
            }),
        ),
        label("选择表情包", (!app.ready).then_some("QQ 尚未就绪")),
    ]
}
