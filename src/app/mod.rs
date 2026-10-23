use crate::{
    backend::{Backend, Event as BackendEvent},
    media::{self, Clipboard},
    model::*,
    runtime::{Config, Paths},
    store::Store,
};
use anyhow::{Context, Result, anyhow};
use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use unicode_segmentation::UnicodeSegmentation;

mod controller;
mod input;
mod work;

pub use controller::run;
#[cfg(test)]
use controller::{Controller, draft_segments};
pub use input::action_labels;
#[cfg(test)]
use work::{RequestContext, Work, WorkSender, apply_receipt, apply_work};
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Conversations,
    Messages,
}
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Viewport {
    pub anchor: Option<String>,
    pub line: usize,
    pub follow: bool,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedUi {
    account: String,
    selected_peer: Option<Peer>,
    views: Vec<(Peer, Viewport, Option<String>)>,
}
fn find_conversation<'a>(items: &'a [Conversation], peer: &Peer) -> Option<&'a Conversation> {
    for c in items {
        if &c.peer == peer {
            return Some(c);
        }
        if let Some(child) = find_conversation(&c.children, peer) {
            return Some(child);
        }
    }
    None
}
fn chat_supported(peer: &Peer) -> bool {
    matches!(peer.chat_type, 1 | 2 | 8 | 100 | 103)
}
#[derive(Default)]
pub struct ReadState {
    pub confirmed: String,
    pub pending: Option<uuid::Uuid>,
    pub retry_after: Option<Instant>,
}
#[derive(Default)]
pub enum UnreadState {
    #[default]
    Uncaptured,
    Loading {
        token: uuid::Uuid,
        jump: bool,
    },
    Ready(Option<String>),
    Failed(String),
}
pub enum QuoteState {
    Loading(uuid::Uuid),
    Ready(Box<Message>),
    Missing,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum PreviewZoom {
    #[default]
    Normal,
    Large,
    Fullscreen,
}
pub struct Chat {
    pub messages: Vec<Message>,
    pub quotes: HashMap<String, QuoteState>,
    pub quote_jump: Option<(String, String)>,
    pub jump_back: Vec<(Viewport, Option<String>)>,
    pub draft: Draft,
    pub draft_loaded: bool,
    pub permission: Option<SpeechPermission>,
    pub permission_pending: bool,
    pub permission_generation: u64,
    pub permission_at: Option<Instant>,
    pub last_used: Instant,
    pub selected: Option<String>,
    pub viewport: Viewport,
    pub loaded: bool,
    pub loading: bool,
    pub pending_history: Option<HistoryPoint>,
    pub exhausted: bool,
    pub has_later: bool,
    pub newest_seen: Option<(u64, u64, u64)>,
    pub generation: u64,
    pub draft_version: u64,
    pub draft_saving: Option<u64>,
    pub dirty_since: Option<Instant>,
    pub read: ReadState,
    pub unread: UnreadState,
    pub new_messages: usize,
    pub rows: Vec<(String, usize, usize)>,
    pub view_height: usize,
    pub view_start: usize,
    pub composer_pending: u32,
    pub composer_generation: u64,
}
impl Default for Chat {
    fn default() -> Self {
        Self {
            messages: vec![],
            quotes: HashMap::new(),
            quote_jump: None,
            jump_back: vec![],
            draft: Draft::default(),
            draft_loaded: false,
            permission: None,
            permission_pending: false,
            permission_generation: 0,
            permission_at: None,
            last_used: Instant::now(),
            selected: None,
            viewport: Viewport {
                follow: true,
                ..Default::default()
            },
            loaded: false,
            loading: false,
            pending_history: None,
            exhausted: false,
            has_later: false,
            newest_seen: None,
            generation: 0,
            draft_version: 0,
            draft_saving: None,
            dirty_since: None,
            read: ReadState::default(),
            unread: UnreadState::default(),
            new_messages: 0,
            rows: vec![],
            view_height: 1,
            view_start: 0,
            composer_pending: 0,
            composer_generation: 0,
        }
    }
}
impl Chat {
    pub fn selected_message(&self) -> Option<&Message> {
        self.selected
            .as_ref()
            .and_then(|id| self.messages.iter().find(|m| &m.id == id))
    }
    pub fn quote_summary(&self, id: &str) -> String {
        self.messages
            .iter()
            .find(|m| m.id == id)
            .or_else(|| match self.quotes.get(id) {
                Some(QuoteState::Ready(message)) => Some(message.as_ref()),
                _ => None,
            })
            .map(|m| {
                let body = if m.text.is_empty() {
                    m.attachments
                        .first()
                        .map(|a| match a.kind.as_str() {
                            "image" => "图片",
                            "sticker" => "表情",
                            "emoji" => "表情",
                            "audio" => "语音",
                            "video" => "视频",
                            _ => a.name.as_str(),
                        })
                        .unwrap_or("消息")
                } else {
                    &m.text
                };
                format!("{}：{}", m.sender, body.replace('\n', " "))
            })
            .unwrap_or_else(|| {
                if matches!(self.quotes.get(id), Some(QuoteState::Missing)) {
                    "原消息不可用".into()
                } else {
                    "正在读取引用…".into()
                }
            })
    }
    pub fn merge(&mut self, messages: Vec<Message>) {
        let previous_last = self.messages.last().map(Message::order_key);
        let mut known: HashMap<String, Message> =
            self.messages.drain(..).map(|m| (m.id.clone(), m)).collect();
        for mut m in messages {
            m.touch();
            for state in self.quotes.values_mut() {
                if let QuoteState::Ready(old) = state
                    && old.id == m.id
                    && (!old.recalled || m.recalled)
                {
                    **old = m.clone();
                }
            }
            if known
                .get(&m.id)
                .is_some_and(|old| old.recalled && !m.recalled)
            {
                continue;
            }
            known.insert(m.id.clone(), m);
        }
        self.messages = known.into_values().collect();
        self.messages.sort_by_key(Message::order_key);
        while self.messages.len() > 1
            && self
                .messages
                .iter()
                .map(Message::memory_bytes)
                .sum::<usize>()
                > 32 * 1024 * 1024
        {
            let anchor = self
                .viewport
                .anchor
                .as_ref()
                .and_then(|id| self.messages.iter().position(|m| &m.id == id))
                .unwrap_or(self.messages.len() - 1);
            if anchor < self.messages.len() / 2 {
                self.messages.pop();
                self.has_later = true;
                self.viewport.follow = false;
            } else {
                self.messages.remove(0);
                self.exhausted = false;
            }
        }
        if self.messages.len() > 600 {
            let center = if self.viewport.follow {
                self.messages.len() - 1
            } else {
                self.viewport
                    .anchor
                    .as_ref()
                    .and_then(|id| self.messages.iter().position(|m| &m.id == id))
                    .unwrap_or(0)
            };
            let from = center.saturating_sub(300).min(self.messages.len() - 600);
            self.exhausted &= from == 0;
            if from + 600 < self.messages.len() {
                self.has_later = true;
                self.viewport.follow = false;
            }
            self.messages = self.messages.drain(from..from + 600).collect();
        }
        if self.selected.is_none()
            || self.viewport.follow && previous_last != self.messages.last().map(Message::order_key)
        {
            self.selected = self.messages.last().map(|m| m.id.clone());
        }
    }
    pub fn receive(&mut self, messages: Vec<Message>) {
        let first = self.messages.first().map(Message::order_key);
        let last = self.messages.last().map(Message::order_key);
        let mut known: HashSet<String> = self.messages.iter().map(|m| m.id.clone()).collect();
        let mut admitted = vec![];
        for message in messages {
            for state in self.quotes.values_mut() {
                if let QuoteState::Ready(old) = state
                    && old.id == message.id
                    && (!old.recalled || message.recalled)
                {
                    **old = message.clone();
                }
            }
            let key = message.order_key();
            let latest = self.newest_seen.max(last);
            if !self.viewport.follow
                && !message.is_self
                && latest.is_some_and(|last| key > last)
                && !known.contains(&message.id)
            {
                self.new_messages += 1;
            }
            self.newest_seen = Some(latest.map_or(key, |last| last.max(key)));
            let in_window = first
                .zip(last)
                .is_some_and(|(first, last)| first <= key && key <= last);
            let append_latest = !self.has_later && last.is_none_or(|last| key > last);
            if !self.loaded || in_window || append_latest {
                known.insert(message.id.clone());
                admitted.push(message);
            }
        }
        if !admitted.is_empty() {
            self.merge(admitted);
        }
    }
    pub fn history_position(&self) -> HistoryPoint {
        if !self.viewport.follow
            && let Some(anchor) = self.viewport.anchor.as_ref().or(self.selected.as_ref())
        {
            HistoryPoint::Around(anchor.clone())
        } else {
            HistoryPoint::Latest
        }
    }
    pub fn replace_history(&mut self, mut messages: Vec<Message>) {
        for message in &mut messages {
            if let Some(old) = self
                .messages
                .iter()
                .find(|old| old.id == message.id && old.recalled)
            {
                *message = old.clone();
            }
        }
        self.messages.clear();
        self.merge(messages);
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !self.messages.iter().any(|m| &m.id == id))
        {
            self.selected = self
                .viewport
                .anchor
                .clone()
                .filter(|id| self.messages.iter().any(|m| &m.id == id))
                .or_else(|| self.messages.last().map(|m| m.id.clone()));
        }
    }
    pub fn edit(&mut self, start: usize, end: usize, text: &str) {
        if start > end
            || end > self.draft.text.len()
            || !self.draft.text.is_char_boundary(start)
            || !self.draft.text.is_char_boundary(end)
        {
            return;
        }
        let delta = text.len() as isize - (end - start) as isize;
        self.draft.mentions.retain_mut(|m| {
            if m.end <= start {
                return true;
            }
            if m.start >= end {
                m.start = m.start.saturating_add_signed(delta);
                m.end = m.end.saturating_add_signed(delta);
                return true;
            }
            false
        });
        self.draft.text.replace_range(start..end, text);
        self.draft.cursor = start + text.len();
        self.changed();
    }
    pub fn changed(&mut self) {
        self.draft_loaded = true;
        self.draft_version += 1;
        self.dirty_since = Some(Instant::now());
    }
    pub fn scroll_lines(&mut self, delta: isize) {
        let Some((_, _, end)) = self.rows.last() else {
            return;
        };
        let start = self
            .view_start
            .saturating_add_signed(delta)
            .min(end.saturating_sub(self.view_height));
        if let Some((id, row, _)) = self
            .rows
            .iter()
            .find(|(_, row, end)| *row <= start && start < *end)
        {
            self.viewport.follow = false;
            self.viewport.anchor = Some(id.clone());
            self.viewport.line = start - row;
            self.view_start = start;
            let selection_visible = self.selected.as_ref().is_some_and(|selected| {
                self.rows.iter().any(|(id, row, end)| {
                    id == selected && *row < start + self.view_height && *end > start
                })
            });
            if !selection_visible {
                self.selected = Some(id.clone());
            }
        }
    }
    fn history_edge(&self, delta: isize) -> Option<HistoryPoint> {
        if self.loading {
            return None;
        }
        if delta < 0
            && !self.exhausted
            && (self.view_start == 0
                || self.selected == self.messages.first().map(|m| m.id.clone()))
        {
            return self
                .messages
                .first()
                .map(|m| HistoryPoint::Before(m.id.clone()));
        }
        if delta > 0
            && self.has_later
            && (self.selected == self.messages.last().map(|m| m.id.clone())
                || self
                    .rows
                    .last()
                    .is_some_and(|(_, _, end)| self.view_start + self.view_height >= *end))
        {
            return self
                .messages
                .last()
                .map(|m| HistoryPoint::After(m.id.clone()));
        }
        None
    }
    pub fn move_selection(&mut self, delta: isize) {
        if self.messages.is_empty() {
            return;
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|id| self.messages.iter().position(|m| &m.id == id))
            .unwrap_or(self.messages.len() - 1);
        let next = index
            .saturating_add_signed(delta)
            .min(self.messages.len() - 1);
        self.selected = Some(self.messages[next].id.clone());
        self.viewport.follow = false;
        if let Some((id, start, end)) = self
            .rows
            .iter()
            .find(|(id, _, _)| id == &self.messages[next].id)
            && (*start < self.view_start || *end > self.view_start + self.view_height)
        {
            self.viewport.anchor = Some(id.clone());
            self.viewport.line = 0;
        }
    }
}
#[derive(Clone, serde::Deserialize)]
pub struct Member {
    pub uid: String,
    pub uin: String,
    pub name: String,
}
#[derive(Clone)]
pub enum EmojiPurpose {
    Send,
    React(Box<Message>),
}
pub fn sticker_choices<'a>(items: &'a [Sticker], query: &str) -> Vec<&'a Sticker> {
    let query = query.to_lowercase();
    items
        .iter()
        .filter(|item| {
            item.name.to_lowercase().contains(&query) || item.id.to_lowercase().contains(&query)
        })
        .collect()
}
#[derive(Clone, Debug)]
pub enum OpenTarget {
    Attachment(Attachment),
    Url(String),
}
impl OpenTarget {
    pub fn label(&self) -> String {
        match self {
            Self::Url(url) => format!("链接  {}", safe_text(url)),
            Self::Attachment(a) => format!(
                "{}  {}",
                if a.is_image_like() {
                    "图片"
                } else {
                    "文件"
                },
                safe_text(a.display_name.as_deref().unwrap_or(&a.name))
            ),
        }
    }
    fn action(&self, message: &Message) -> Action {
        match self {
            Self::Url(url) => Action::Open(url.clone()),
            Self::Attachment(a) => Action::Resolve(message.clone(), a.clone()),
        }
    }
}
pub fn message_objects(message: &Message) -> Vec<OpenTarget> {
    let mut items = vec![];
    let mut urls = HashSet::new();
    let mut attachments = HashSet::new();
    for part in message.render_parts() {
        match part {
            MessagePart::Text { text } => {
                for url in message_urls(&text) {
                    if urls.insert(url.clone()) {
                        items.push(OpenTarget::Url(url));
                    }
                }
            }
            MessagePart::Attachment { element_id } => {
                if attachments.insert(element_id.clone())
                    && let Some(a) = message
                        .attachments
                        .iter()
                        .find(|a| a.element_id == element_id)
                {
                    items.push(OpenTarget::Attachment(a.clone()));
                }
            }
        }
    }
    items
}
fn open_objects(message: Message, items: Vec<OpenTarget>) -> (Modal, Vec<Action>) {
    if items.is_empty() {
        return (Modal::None, vec![]);
    }
    if items.len() == 1 {
        (Modal::None, vec![items[0].action(&message)])
    } else {
        (
            Modal::Objects {
                message,
                items,
                index: 0,
            },
            vec![],
        )
    }
}
pub enum Modal {
    Objects {
        message: Message,
        items: Vec<OpenTarget>,
        index: usize,
    },
    EmojiPicker {
        token: uuid::Uuid,
        purpose: EmojiPurpose,
        items: Vec<Sticker>,
        query: String,
        index: usize,
    },
    Reactions {
        message: Message,
        index: usize,
    },
    ReactionMembers {
        message: Message,
        reaction: Reaction,
        page: ReactionPage,
        index: usize,
    },
    Forward {
        root: Message,
        path: Vec<String>,
        messages: Vec<Message>,
        index: usize,
        scroll: usize,
    },
    None,
    Loading(String),
    Switcher {
        query: String,
        index: usize,
    },
    Search {
        query: String,
        index: usize,
        results: Vec<Message>,
    },
    Help,
    Actions {
        index: usize,
    },
    ConversationActions {
        conversation: Conversation,
        index: usize,
    },
    AttachmentPath {
        input: String,
        sticker: bool,
    },
    Attachments {
        message: Message,
        index: usize,
    },
    Preview {
        message: Message,
        attachment: Attachment,
        zoom: PreviewZoom,
    },
    Members {
        query: String,
        index: usize,
        members: Vec<Member>,
        mention: bool,
    },
    Contacts {
        query: String,
        index: usize,
        items: Vec<Conversation>,
    },
    ConfirmRecall(Message),
    Outbox {
        index: usize,
        items: Vec<(Receipt, Draft)>,
    },
    Profile(Profile),
}
impl Modal {
    pub fn accepts_text(&self) -> bool {
        matches!(
            self,
            Self::EmojiPicker { .. }
                | Self::Switcher { .. }
                | Self::Search { .. }
                | Self::AttachmentPath { .. }
                | Self::Members { .. }
                | Self::Contacts { .. }
        )
    }
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
}
#[derive(Clone, Debug)]
pub enum HistoryPoint {
    Latest,
    Before(String),
    After(String),
    Around(String),
}
pub enum Action {
    EmojiCatalog(EmojiPurpose),
    ChooseSticker(Sticker),
    Reactions(Message),
    SetReaction {
        message: Message,
        reaction: Reaction,
        set: bool,
    },
    ReactionMembers {
        message: Message,
        reaction: Reaction,
        cookie: Option<String>,
    },
    Forward {
        root: Message,
        path: Vec<String>,
    },
    Select(Peer),
    History(HistoryPoint),
    JumpQuote(Message),
    JumpBack,
    FirstUnread,
    Snapshot,
    MarkRead(Conversation),
    Folder,
    Profile(String),
    Send,
    Copy(String),
    Paste,
    Attach {
        file: String,
        sticker: bool,
    },
    Members(bool),
    Contacts,
    Search(String),
    Recall(Message),
    RestoreDraft(Receipt, Draft),
    Resolve(Message, Attachment),
    Open(String),
    Quit,
}
pub struct App {
    context: uuid::Uuid,
    pub account: String,
    pub instance_account: Option<String>,
    pub epoch: String,
    pub seq: u64,
    pub conversations: Vec<Conversation>,
    pub notifications: HashMap<String, crate::notifications::Notice>,
    pub unread_total: Option<u64>,
    pub selected_peer: Option<Peer>,
    pub sidebar_cursor: Option<Peer>,
    pub sidebar_offset: usize,
    pub hit_regions: crate::ui::HitRegions,
    pub render_cache: crate::ui::RenderCache,
    pub chats: HashMap<Peer, Chat>,
    pub mode: Mode,
    pub focus: Focus,
    pub modal: Modal,
    pub pending_dialog: Option<uuid::Uuid>,
    pub modal_return: Option<Box<Modal>>,
    pub forward_parents: Vec<Modal>,
    pub forward_rows: Vec<(usize, usize)>,
    pub forward_area: ratatui::layout::Rect,
    pub forward_reveal: bool,
    pub config: Config,
    pub terminal_focused: bool,
    pub notice: String,
    pub connected: bool,
    pub ready: bool,
    pub sync_needed: bool,
    pub syncing: bool,
    pub sync_urgent: bool,
    pub staging: bool,
    pub queued_send: Option<(Peer, u64)>,
    pub pending_account: Option<uuid::Uuid>,
    pub media_reset: bool,
    pub receipts: HashMap<String, (Receipt, Draft)>,
    pub avatars: std::rc::Rc<HashMap<String, Result<Avatar, String>>>,
    pub avatar_pending: HashMap<String, uuid::Uuid>,
    pub media_invalidations: Vec<String>,
    pub failure_times: HashMap<String, Instant>,
    pub notice_deadline: Option<(String, Instant)>,
    pub pending_images: HashMap<String, bool>,
    pub failed_images: HashMap<String, String>,
    pub sticker_previews: HashSet<(uuid::Uuid, String)>,
    pub sticker_errors: HashMap<(uuid::Uuid, String), String>,
    pub reaction_refreshes: HashSet<(Peer, String)>,
    pub capabilities: HashSet<String>,
    pub dirty: bool,
    pub quit: bool,
    pub key_prefix: String,
    pub prefix_at: Instant,
}
impl App {
    pub fn new(config: Config) -> Self {
        Self {
            context: uuid::Uuid::new_v4(),
            account: String::new(),
            instance_account: None,
            epoch: String::new(),
            seq: 0,
            conversations: vec![],
            notifications: Default::default(),
            unread_total: None,
            selected_peer: None,
            sidebar_cursor: None,
            sidebar_offset: 0,
            hit_regions: Default::default(),
            render_cache: Default::default(),
            chats: HashMap::new(),
            mode: Mode::Normal,
            focus: Focus::Conversations,
            modal: Modal::None,
            pending_dialog: None,
            modal_return: None,
            forward_parents: vec![],
            forward_rows: vec![],
            forward_area: Default::default(),
            forward_reveal: false,
            config,
            terminal_focused: false,
            notice: "正在连接 QQ 宿主…".into(),
            connected: false,
            ready: false,
            sync_needed: false,
            syncing: false,
            sync_urgent: true,
            staging: false,
            queued_send: None,
            pending_account: None,
            media_reset: false,
            receipts: HashMap::new(),
            avatars: std::rc::Rc::default(),
            avatar_pending: HashMap::new(),
            media_invalidations: vec![],
            failure_times: HashMap::new(),
            notice_deadline: None,
            pending_images: Default::default(),
            failed_images: Default::default(),
            sticker_previews: Default::default(),
            sticker_errors: Default::default(),
            reaction_refreshes: Default::default(),
            capabilities: Default::default(),
            dirty: true,
            quit: false,
            key_prefix: String::new(),
            prefix_at: Instant::now(),
        }
    }
    pub fn login_command(&self) -> String {
        self.instance_account.as_ref().map_or_else(
            || "qqtui backend view".into(),
            |account| format!("qqtui --account {account} backend view"),
        )
    }
    pub fn begin_dialog(&mut self, label: &str) -> uuid::Uuid {
        let token = uuid::Uuid::new_v4();
        self.pending_dialog = Some(token);
        self.modal = Modal::Loading(label.into());
        self.dirty = true;
        token
    }
    pub fn complete_dialog(&mut self, token: uuid::Uuid) -> bool {
        if self.pending_dialog != Some(token) {
            return false;
        }
        self.pending_dialog = None;
        true
    }
    pub fn chat(&self) -> Option<&Chat> {
        self.selected_peer.as_ref().and_then(|p| self.chats.get(p))
    }
    pub fn chat_mut(&mut self) -> Option<&mut Chat> {
        self.selected_peer
            .as_ref()
            .and_then(|p| self.chats.get_mut(p))
    }
    pub fn draft_editable(&mut self) -> bool {
        if self.pending_account.is_some() {
            self.notice = "正在保存旧账号草稿，请稍候".into();
            return false;
        }
        if self.selected_peer.as_ref().is_some_and(|p| p.is_group())
            && self.capabilities.contains("speech_permissions")
            && self
                .chat()
                .is_some_and(|c| c.permission.as_ref().is_none_or(|p| p.state != "allowed"))
        {
            self.notice = self
                .chat()
                .and_then(|c| c.permission.as_ref())
                .map_or_else(|| "正在确认群发言权限".into(), |p| p.reason.clone());
            return false;
        }
        if self.selected_peer.as_ref().is_some_and(|p| p.is_mailbox()) {
            self.notice = "QQ 邮箱提醒仅支持查看".into();
            return false;
        }
        if self.chat().is_some_and(|chat| chat.draft_loaded) {
            return true;
        }
        self.notice = "草稿尚未读取，暂不能编辑；可切换会话重试".into();
        false
    }
    pub fn name(&self) -> String {
        self.selected_peer
            .as_ref()
            .and_then(|p| find_conversation(&self.conversations, p))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "选择会话".into())
    }
    pub fn switcher_items(&self, query: &str) -> Vec<&Conversation> {
        let q = query.to_lowercase();
        fn collect<'a>(items: &'a [Conversation], q: &str, matches: &mut Vec<&'a Conversation>) {
            for c in items {
                if c.name.to_lowercase().contains(q) || c.peer.uid.to_lowercase().contains(q) {
                    matches.push(c);
                }
                collect(&c.children, q, matches);
            }
        }
        let mut matches = vec![];
        collect(&self.conversations, &q, &mut matches);
        matches
    }
    pub(crate) fn reset_native_pending(&mut self) {
        self.pending_images.clear();
        self.failed_images.clear();
        self.sticker_previews.clear();
        self.sticker_errors.clear();
        self.reaction_refreshes.clear();
        for chat in self.chats.values_mut() {
            chat.quotes.retain(|_, q| matches!(q, QuoteState::Ready(_)));
            chat.quote_jump = None;
            if matches!(chat.unread, UnreadState::Loading { .. }) {
                chat.unread = UnreadState::Uncaptured;
            }
        }
    }
    pub(crate) fn close_lifecycle_modals(&mut self) {
        if matches!(
            self.modal,
            Modal::Loading(_)
                | Modal::EmojiPicker { .. }
                | Modal::Reactions { .. }
                | Modal::ReactionMembers { .. }
        ) {
            self.pending_dialog = None;
            self.modal = Modal::None;
        }
    }
    pub fn operation_error(&mut self, error: impl std::fmt::Display) {
        let text = error.to_string();
        self.notice_deadline = Some((text.clone(), Instant::now() + Duration::from_secs(5)));
        self.notice = text;
        self.dirty = true;
    }
    pub fn expire_notices(&mut self, now: Instant) {
        if self
            .notice_deadline
            .as_ref()
            .is_some_and(|(_, t)| *t <= now)
        {
            if let Some((text, _)) = self.notice_deadline.take()
                && text == self.notice
            {
                self.notice.clear();
            }
            self.dirty = true;
        }
        let old = self.failure_times.len();
        self.failure_times
            .retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(5));
        self.dirty |= old != self.failure_times.len();
    }
    pub fn bound_windows(&mut self) {
        while self.receipts.len() > 256 {
            let candidate = self
                .receipts
                .iter()
                .find(|(_, (r, _))| matches!(r.state, SendState::Confirmed | SendState::Failed))
                .or_else(|| {
                    self.receipts
                        .iter()
                        .find(|(_, (r, _))| r.state == SendState::Unknown)
                })
                .map(|(id, _)| id.clone());
            let Some(id) = candidate else {
                break;
            };
            self.receipts.remove(&id);
        }
        while self.sticker_errors.len() > 128 {
            let Some(key) = self.sticker_errors.keys().next().cloned() else {
                break;
            };
            self.sticker_errors.remove(&key);
        }

        let mut windows = self
            .chats
            .iter()
            .filter(|(_, c)| !c.messages.is_empty())
            .map(|(p, c)| {
                (
                    p.clone(),
                    c.last_used,
                    c.messages.iter().map(Message::memory_bytes).sum::<usize>(),
                )
            })
            .collect::<Vec<_>>();
        windows.sort_by_key(|(_, time, _)| *time);
        let mut total = windows.iter().map(|(_, _, b)| b).sum::<usize>();
        let mut count = windows.len();
        for (peer, _, bytes) in windows {
            if count <= 8 && total <= 32 * 1024 * 1024 {
                break;
            }
            if self.selected_peer.as_ref() == Some(&peer) {
                continue;
            }
            if let Some(c) = self.chats.get_mut(&peer) {
                c.messages.clear();
                c.quotes.clear();
                c.loaded = false;
                c.rows.clear();
                count -= 1;
                total = total.saturating_sub(bytes);
            }
        }
    }
    pub fn binding(&self, action: &str) -> String {
        self.config.keymap.get(action).cloned().unwrap_or_else(|| {
            match action {
                "left" => "h",
                "right" => "l",
                "next" => "j",
                "next_unread" => "]q",
                "previous_unread" => "[q",
                "previous" => "k",
                "top" => "gg",
                "bottom" => "G",
                "page_up" => "ctrl-u",
                "page_down" => "ctrl-d",
                "switch" => "ctrl-p",
                "reply" => "r",
                "copy" => "y",
                "insert" => "i",
                "search" => "/",
                "help" => "?",
                "quit" => "q",
                "actions" => "space",
                "members" => "m",
                "contacts" => "c",
                "attach" => "a",
                "sticker_file" => "A",
                "clear" => "x",
                "send" => "enter",
                "newline" => "alt-enter",
                "paste" => "ctrl-v",
                "sidebar" => "ctrl-b",
                "narrower" => "alt-h",
                "wider" => "alt-l",
                "outbox" => "ctrl-o",
                "profile" => "p",
                "preview" => "v",
                "open" => "o",
                "quote" => "gr",
                "first_unread" => "gu",
                "back" => "ctrl-t",
                "stickers" => "ctrl-e",
                "reactions" => "R",
                _ => "",
            }
            .into()
        })
    }
    fn change_account(&mut self, account: String) {
        self.context = uuid::Uuid::new_v4();
        self.account = account;
        self.epoch.clear();
        self.seq = 0;
        self.ready = false;
        self.syncing = false;
        self.sync_needed = true;
        self.sync_urgent = true;
        self.chats.clear();
        self.conversations.clear();
        self.selected_peer = None;
        self.sidebar_cursor = None;
        self.sidebar_offset = 0;
        self.render_cache = Default::default();
        std::rc::Rc::make_mut(&mut self.avatars).clear();
        self.avatar_pending.clear();
        self.media_invalidations.clear();
        self.media_reset = true;
        self.forward_parents.clear();
        self.forward_rows.clear();
        self.forward_area = Default::default();
        self.forward_reveal = false;
        self.modal = Modal::None;
        self.modal_return = None;
        self.pending_dialog = None;
        self.pending_account = None;
        self.reset_native_pending();
        self.receipts.clear();
        self.capabilities.clear();
        self.notifications.clear();
        self.failure_times.clear();
        self.notice_deadline = None;
        self.notice.clear();
        self.unread_total = None;
        self.queued_send = None;
        self.staging = false;
        self.key_prefix.clear();
        self.mode = Mode::Normal;
        self.focus = Focus::Conversations;
    }
    pub fn apply_snapshot(&mut self, s: Snapshot) {
        let previous = self.selected_peer.clone();
        if self.account != s.account {
            self.change_account(s.account.clone());
        }
        self.seq = if self.epoch == s.epoch {
            self.seq.max(s.seq)
        } else {
            s.seq
        };
        self.account = s.account;
        self.epoch = s.epoch;
        self.unread_total = s.unread_total.and_then(|n| n.parse().ok());
        self.conversations = s.conversations;
        if self
            .selected_peer
            .as_ref()
            .is_none_or(|p| find_conversation(&self.conversations, p).is_none())
        {
            self.selected_peer = self.conversations.first().map(|c| c.peer.clone());
        }
        if self
            .sidebar_cursor
            .as_ref()
            .is_none_or(|p| !self.conversations.iter().any(|c| &c.peer == p))
        {
            self.sidebar_cursor = self.selected_peer.clone();
        }
        if let Some(p) = &self.selected_peer {
            self.chats.entry(p.clone()).or_default();
        }
        if self.selected_peer != previous {
            self.pending_dialog = None;
            self.modal = Modal::None;
        }
        self.syncing = false;
        self.dirty = true;
    }
}
pub fn previous_boundary(s: &str, pos: usize) -> usize {
    s[..pos]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}
pub fn next_boundary(s: &str, pos: usize) -> usize {
    s[pos..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(i, _)| pos + i)
        .unwrap_or(s.len())
}
fn update_forward_media(
    messages: &mut [Message],
    account: &str,
    peer: &Peer,
    id: &str,
    element: &str,
    forward_path: &[String],
    resource: &ResolvedMedia,
) {
    if let Some(message) = messages.iter_mut().find(|m| {
        m.account == account && &m.peer == peer && m.id == id && m.forward_path == forward_path
    }) && let Some(attachment) = message
        .attachments
        .iter_mut()
        .find(|a| a.element_id == element)
    {
        attachment.path = resource.path.clone();
        attachment.warning = resource.warning.clone();
        message.touch();
    }
}
pub fn message_urls(text: &str) -> Vec<String> {
    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]);
    finder
        .links(text)
        .filter_map(|link| url::Url::parse(link.as_str()).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(Into::into)
        .collect()
}
pub fn first_url(text: &str) -> Option<String> {
    message_urls(text).into_iter().next()
}
pub fn word_boundary(text: &str, pos: usize, forward: bool) -> usize {
    let starts = text.unicode_word_indices().map(|(index, _)| index);
    if forward {
        starts
            .filter(|index| *index > pos)
            .min()
            .unwrap_or(text.len())
    } else {
        starts.filter(|index| *index < pos).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_opens_group_folder_and_folder_rows_show_unread_counts() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new(Config::default());
        let folder = Peer {
            chat_type: 7,
            uid: "folder".into(),
        };
        let child = Conversation {
            name: "未读群".into(),
            unread: "12".into(),
            ..conversation("group")
        };
        app.conversations = vec![Conversation {
            peer: folder.clone(),
            children: vec![child],
            ..conversation("folder")
        }];
        let rect = ratatui::layout::Rect::new(0, 0, 20, 1);
        app.hit_regions.conversations = vec![(rect, folder)];
        assert!(
            app.terminal(TerminalEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 0,
                modifiers: KeyModifiers::NONE
            }))
            .is_empty()
        );
        assert!(matches!(app.modal, Modal::Contacts { .. }));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut media);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered
                .split_whitespace()
                .collect::<String>()
                .contains("未读群12")
        );
    }
    #[test]
    fn moderator_can_recall_others_but_members_and_private_chats_cannot() {
        for (chat_type, is_self, can_moderate, allowed) in [
            (2, false, true, true),
            (2, false, false, false),
            (2, true, false, true),
            (1, false, true, false),
        ] {
            let mut app = App::new(Config::default());
            app.ready = true;
            let group = Peer {
                chat_type,
                uid: "group".into(),
            };
            app.selected_peer = Some(group.clone());
            let message = Message {
                peer: group.clone(),
                is_self,
                ..msg("1", "1")
            };
            app.chats.insert(
                group,
                Chat {
                    selected: Some("1".into()),
                    messages: vec![message],
                    permission: Some(SpeechPermission {
                        can_moderate,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            assert_eq!(action_labels(&app)[2].1, allowed);
        }
    }
    #[test]
    fn emoji_picker_displays_a_grid_and_arrows_and_mouse_select_the_same_cells() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("face.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let mut app = App::new(Config::default());
        app.modal = Modal::EmojiPicker {
            token: uuid::Uuid::new_v4(),
            purpose: EmojiPurpose::Send,
            items: (0..40)
                .map(|i| Sticker {
                    id: i.to_string(),
                    name: format!("表情{i}"),
                    kind: "sticker".into(),
                    path: path.to_string_lossy().into_owned(),
                    emoji_id: String::new(),
                    emoji_type: String::new(),
                })
                .collect(),
            query: String::new(),
            index: 0,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut images = 0;
        terminal
            .draw(|frame| {
                images = crate::ui::draw(frame, &mut app, &mut media).images.len();
            })
            .unwrap();
        assert!(images >= 12);
        assert!(app.hit_regions.emoji_columns > 1);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(
            matches!(&app.modal, Modal::EmojiPicker {index, query, ..} if *index == app.hit_regions.emoji_columns && query.is_empty())
        );
        let (cell, selected) = app.hit_regions.emoji_cells[2];
        app.terminal(TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: cell.x + 1,
            row: cell.y + 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(matches!(&app.modal, Modal::EmojiPicker {index, ..} if *index == selected));
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(
            matches!(&app.modal, Modal::EmojiPicker {query, index, ..} if query == "j" && *index == 0)
        );
    }
    fn peer(uid: &str) -> Peer {
        Peer {
            chat_type: 1,
            uid: uid.into(),
        }
    }
    fn msg(id: &str, time: &str) -> Message {
        Message {
            account: "a".into(),
            peer: peer("self"),
            id: id.into(),
            time: time.into(),
            seq: time.into(),
            ..Default::default()
        }
    }
    fn conversation(uid: &str) -> Conversation {
        Conversation {
            peer: peer(uid),
            id: uid.into(),
            name: uid.into(),
            unread: "0".into(),
            ..Default::default()
        }
    }
    #[test]
    fn reactions_use_explicit_desired_state_and_keep_message_selection_on_metadata_updates() {
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let mut target = msg("1", "1");
        target.peer.chat_type = 2;
        target.reactions = vec![Reaction {
            emoji_id: "76".into(),
            emoji_type: "1".into(),
            count: "2".into(),
            clicked: true,
            label: "赞".into(),
            ..Default::default()
        }];
        app.modal = Modal::Reactions {
            message: target.clone(),
            index: 0,
        };
        let actions = app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(matches!(
            actions.as_slice(),
            [Action::SetReaction { set: false, .. }]
        ));
        assert_eq!(target.reactions[0].count, "2");
        let mut chat = Chat::default();
        chat.merge(vec![target.clone(), msg("2", "2")]);
        chat.selected = Some("1".into());
        target.reactions[0].count = "3".into();
        chat.receive(vec![target]);
        assert_eq!(chat.selected.as_deref(), Some("1"));
        assert_eq!(chat.messages[0].reactions[0].count, "3");
    }
    #[tokio::test]
    async fn notification_activation_keeps_account_and_message_identity_and_suspends_follow() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store,
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let mut target = crate::notifications::Target {
            account: "b".into(),
            peer: peer("target"),
            id: "300".into(),
        };
        controller.activate_notification(&mut app, target.clone());
        assert!(app.selected_peer.is_none());
        app.chats.entry(target.peer.clone()).or_default().draft.text = "draft".into();
        target.account = "a".into();
        controller.activate_notification(&mut app, target);
        assert_eq!(app.selected_peer, Some(peer("target")));
        let chat = app.chat().unwrap();
        assert_eq!(chat.viewport.anchor.as_deref(), Some("300"));
        assert!(!chat.viewport.follow);
        assert_eq!(chat.draft.text, "draft");
        assert!(app.focus == Focus::Messages);
    }
    #[tokio::test]
    async fn sticker_cancel_and_late_preview_results_cannot_modify_the_new_picker_or_draft() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.draft.text = "keep".into();
        chat.composer_pending = 1;
        app.chats.insert(peer("self"), chat);
        let canceled = app.begin_dialog("read sticker");
        app.terminal(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::StickerChosen {
                token: canceled,
                peer: peer("self"),
                result: Ok(Attachment {
                    kind: "sticker".into(),
                    name: "late.gif".into(),
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().draft.text, "keep");
        assert!(app.chat().unwrap().draft.attachments.is_empty());
        assert_eq!(app.chat().unwrap().composer_pending, 0);
        let token = uuid::Uuid::new_v4();
        app.modal = Modal::EmojiPicker {
            token,
            purpose: EmojiPurpose::Send,
            items: vec![Sticker {
                id: "favorite:1".into(),
                kind: "sticker".into(),
                name: "test".into(),
                path: String::new(),
                emoji_id: String::new(),
                emoji_type: String::new(),
            }],
            query: String::new(),
            index: 0,
        };
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::StickerPreview {
                token: canceled,
                id: "favorite:1".into(),
                result: Ok("/stale.gif".into()),
            },
        );
        assert!(matches!(&app.modal,Modal::EmojiPicker {items,..} if items[0].path.is_empty()));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::StickerPreview {
                token,
                id: "favorite:1".into(),
                result: Err(anyhow!("unavailable")),
            },
        );
        assert!(
            app.sticker_errors
                .contains_key(&(token, "favorite:1".into()))
        );
    }
    #[tokio::test]
    async fn choosing_a_qq_face_only_updates_the_draft_and_uses_the_existing_send_gate() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store,
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        app.chats.insert(
            peer("self"),
            Chat {
                draft_loaded: true,
                ..Default::default()
            },
        );
        controller.action(
            &mut app,
            Action::ChooseSticker(Sticker {
                id: "face:14".into(),
                kind: "face".into(),
                name: "微笑".into(),
                path: String::new(),
                emoji_id: "14".into(),
                emoji_type: "1".into(),
            }),
        );
        assert_eq!(app.chat().unwrap().draft.attachments[0].kind, "face");
        assert!(app.receipts.is_empty());
        assert!(!app.staging);
        assert!(app.mode == Mode::Insert);
    }
    #[test]
    fn conversation_navigation_waits_for_enter_and_preserves_cursor_by_peer() {
        let mut app = App::new(Config::default());
        app.conversations = vec![
            conversation("one"),
            conversation("two"),
            conversation("three"),
        ];
        app.selected_peer = Some(peer("one"));
        app.sidebar_cursor = app.selected_peer.clone();
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        assert!(app.key(key('j')).is_empty());
        assert_eq!(app.sidebar_cursor, Some(peer("two")));
        assert_eq!(app.selected_peer, Some(peer("one")));
        app.conversations.swap(0, 1);
        assert!(app.key(key('j')).is_empty());
        assert_eq!(app.sidebar_cursor, Some(peer("one")));
        app.key(key('G'));
        assert_eq!(app.sidebar_cursor, Some(peer("three")));
        assert_eq!(app.selected_peer, Some(peer("one")));
        let actions = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(actions.as_slice(), [Action::Select(p)] if *p == peer("three")));
        assert!(app.focus == Focus::Messages);
    }
    #[test]
    fn mouse_hits_rendered_rows_and_wheel_fetches_older_pages_without_clicking_through_modals() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = App::new(Config::default());
        app.conversations = vec![conversation("one"), conversation("two")];
        app.selected_peer = Some(peer("one"));
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(
            (1..=40)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        app.chats.insert(peer("one"), chat);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        let mouse = |kind, x, y| {
            TerminalEvent::Mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        let (rect, id) = app.hit_regions.messages.first().unwrap().clone();
        app.terminal(mouse(
            MouseEventKind::Down(MouseButton::Left),
            rect.x + 1,
            rect.y,
        ));
        assert!(app.focus == Focus::Messages);
        assert_eq!(app.chat().unwrap().selected, Some(id));
        let mut fetched = false;
        for _ in 0..60 {
            let actions = app.terminal(mouse(MouseEventKind::ScrollUp, rect.x + 1, rect.y));
            fetched |= matches!(actions.as_slice(), [Action::History(HistoryPoint::Before(id))] if id == "1");
            terminal
                .draw(|f| {
                    crate::ui::draw(f, &mut app, &mut media);
                })
                .unwrap();
        }
        assert!(fetched);
        let (row, target) = app.hit_regions.conversations[1].clone();
        let actions = app.terminal(mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
        ));
        assert!(matches!(actions.as_slice(), [Action::Select(p)] if *p == target));
        app.modal = Modal::Help;
        assert!(
            app.terminal(mouse(
                MouseEventKind::Down(MouseButton::Left),
                row.x + 1,
                row.y
            ))
            .is_empty()
        );
    }
    #[test]
    fn reaching_latest_bottom_resumes_follow_and_hints_belong_to_message_focus() {
        let mut app = App::new(Config::default());
        app.terminal_focused = true;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(
            (1..=40)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        chat.viewport.follow = false;
        chat.viewport.anchor = Some("1".into());
        chat.new_messages = 1;
        app.chats.insert(peer("self"), chat);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let screen = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert!(!screen(&terminal).replace(' ', "").contains("G回到底部"));
        app.focus = Focus::Messages;
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert!(screen(&terminal).replace(' ', "").contains("G回到底部"));
        app.chat_mut().unwrap().scroll_lines(10000);
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert!(app.chat().unwrap().viewport.follow);
        let mut incoming = msg("41", "41");
        incoming.text = "new-at-bottom".into();
        app.chat_mut().unwrap().receive(vec![incoming]);
        let mut work = crate::ui::DrawWork::default();
        terminal
            .draw(|f| {
                work = crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert!(work.readable);
        let chat = app.chat().unwrap();
        assert!(chat.viewport.follow);
        assert_eq!(
            chat.view_start + chat.view_height,
            chat.rows.last().unwrap().2
        );
        assert_eq!(chat.new_messages, 0);
        assert!(screen(&terminal).contains("new-at-bottom"));

        app.focus = Focus::Conversations;
        let mut draw = |app: &mut App| {
            terminal
                .draw(|frame| {
                    crate::ui::draw(frame, app, &mut media);
                })
                .unwrap();
        };
        app.terminal(TerminalEvent::FocusLost);
        draw(&mut app);
        assert!(!app.chat().unwrap().viewport.follow);
        app.terminal(TerminalEvent::FocusGained);
        draw(&mut app);
        assert!(app.chat().unwrap().viewport.follow);

        let start = app.chat().unwrap().view_start;
        let anchor = app.chat().unwrap().viewport.anchor.clone();
        let selected = app.chat().unwrap().selected.clone();
        app.terminal(TerminalEvent::FocusLost);
        app.chat_mut().unwrap().receive(vec![msg("42", "42")]);
        draw(&mut app);
        let chat = app.chat().unwrap();
        assert!(!chat.viewport.follow);
        assert_eq!(chat.view_start, start);
        assert_eq!(chat.viewport.anchor, anchor);
        assert_eq!(chat.selected, selected);
        assert_eq!(chat.new_messages, 1);
        app.terminal(TerminalEvent::FocusGained);
        draw(&mut app);
        assert!(!app.chat().unwrap().viewport.follow);
        assert_eq!(app.chat().unwrap().view_start, start);
        assert_eq!(app.chat().unwrap().new_messages, 1);

        app.focus = Focus::Messages;
        app.terminal(TerminalEvent::Key(KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::SHIFT,
        )));
        draw(&mut app);
        assert!(app.chat().unwrap().viewport.follow);
        assert_eq!(app.chat().unwrap().new_messages, 0);

        app.chat_mut().unwrap().scroll_lines(-5);
        draw(&mut app);
        let start = app.chat().unwrap().view_start;
        app.terminal(TerminalEvent::FocusLost);
        app.terminal(TerminalEvent::FocusGained);
        draw(&mut app);
        assert!(!app.chat().unwrap().viewport.follow);
        assert_eq!(app.chat().unwrap().view_start, start);
    }
    #[test]
    fn unresolved_jump_anchor_is_not_replaced_by_old_bottom_rows() {
        for loading in [false, true] {
            let mut app = App::new(Config::default());
            app.selected_peer = Some(peer("self"));
            let mut chat = Chat {
                loaded: true,
                loading,
                ..Default::default()
            };
            chat.merge(vec![msg("105", "105")]);
            chat.selected = Some("5".into());
            chat.viewport = Viewport {
                anchor: Some("5".into()),
                line: 0,
                follow: false,
            };
            app.chats.insert(peer("self"), chat);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
            terminal
                .draw(|frame| {
                    assert!(!crate::ui::draw(frame, &mut app, &mut media).readable);
                })
                .unwrap();
            let chat = app.chat().unwrap();
            assert!(!chat.viewport.follow);
            assert_eq!(chat.viewport.anchor.as_deref(), Some("5"));
            assert_eq!(chat.selected.as_deref(), Some("5"));
        }
    }
    #[test]
    fn native_order_updates_keep_selected_identity() {
        let mut app = App::new(Config::default());
        app.apply_snapshot(Snapshot {
            account: "a".into(),
            unread_total: None,
            epoch: "one".into(),
            seq: 10,
            conversations: vec![conversation("x"), conversation("y")],
        });
        app.selected_peer = Some(peer("y"));
        app.apply_snapshot(Snapshot {
            account: "a".into(),
            unread_total: None,
            epoch: "one".into(),
            seq: 11,
            conversations: vec![conversation("y"), conversation("x")],
        });
        assert_eq!(app.selected_peer, Some(peer("y")));
        assert_eq!(app.conversations[0].peer, peer("y"));
    }
    #[test]
    fn host_restart_resets_sequence_and_account_switch_clears_state() {
        let mut app = App::new(Config::default());
        app.apply_snapshot(Snapshot {
            account: "a".into(),
            unread_total: None,
            epoch: "one".into(),
            seq: 100,
            conversations: vec![conversation("x")],
        });
        app.apply_snapshot(Snapshot {
            account: "a".into(),
            unread_total: None,
            epoch: "two".into(),
            seq: 1,
            conversations: vec![conversation("x")],
        });
        assert_eq!(app.seq, 1);
        app.chat_mut().unwrap().draft.text = "private".into();
        app.apply_snapshot(Snapshot {
            unread_total: None,
            account: "b".into(),
            epoch: "three".into(),
            seq: 0,
            conversations: vec![conversation("y")],
        });
        assert!(app.chat().unwrap().draft.text.is_empty());
        assert!(!app.chats.contains_key(&peer("x")));
    }
    #[test]
    fn history_prepend_and_new_messages_preserve_reading_anchor() {
        let mut c = Chat::default();
        c.merge(vec![msg("2", "2"), msg("3", "3")]);
        c.viewport.follow = false;
        c.viewport.anchor = Some("2".into());
        c.selected = Some("2".into());
        c.merge(vec![msg("1", "1"), msg("3", "3")]);
        c.receive(vec![msg("4", "4")]);
        assert_eq!(
            c.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "3", "4"]
        );
        assert_eq!(c.viewport.anchor.as_deref(), Some("2"));
        assert_eq!(c.selected.as_deref(), Some("2"));
        assert_eq!(c.new_messages, 1);
    }
    #[test]
    fn editing_uses_graphemes_and_preserves_valid_mentions() {
        let mut c = Chat::default();
        c.draft.text = "中👨‍👩‍👧‍👦文".into();
        c.draft.cursor = "中👨‍👩‍👧‍👦".len();
        let p = previous_boundary(&c.draft.text, c.draft.cursor);
        c.edit(p, c.draft.cursor, "");
        assert_eq!(c.draft.text, "中文");
        c.draft = Draft {
            text: "@张三 hi".into(),
            mentions: vec![Mention {
                start: 0,
                end: "@张三".len(),
                uid: "u_member".into(),
                uin: "42".into(),
            }],
            ..Default::default()
        };
        c.edit(0, 0, "a");
        assert_eq!(c.draft.mentions[0].start, 1);
        assert_eq!(draft_segments(&c.draft)[1]["uid"], "u_member");
        c.edit(2, 5, "");
        assert!(c.draft.mentions.is_empty());
    }
    #[tokio::test]
    async fn clear_composer_discards_all_fields_and_late_attachments_but_not_typed_x() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            draft_loaded: true,
            composer_pending: 2,
            ..Default::default()
        };
        chat.draft.text = "old".into();
        chat.draft.cursor = 3;
        chat.draft.reply_to = Some("1".into());
        chat.draft.attachments.push(Attachment::default());
        app.chats.insert(peer("self"), chat);
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_empty()
        );
        assert_eq!(
            serde_json::to_value(&app.chat().unwrap().draft).unwrap(),
            serde_json::to_value(Draft::default()).unwrap()
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Attachment {
                peer: peer("self"),
                generation: 0,
                result: Ok(Attachment::default()),
            },
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Clipboard {
                peer: peer("self"),
                generation: 0,
                result: Ok(Clipboard::Text("late".into())),
            },
        );
        assert!(app.chat().unwrap().draft.text.is_empty());
        assert!(app.chat().unwrap().draft.attachments.is_empty());
        assert_eq!(app.chat().unwrap().composer_pending, 0);
        app.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.text, "x");
        app.mode = Mode::Normal;
        app.staging = true;
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.text, "x");
    }
    #[test]
    fn sidebar_text_stays_in_the_same_column_with_group_folder_selection() {
        let mut app = App::new(Config::default());
        app.conversations = vec![conversation("one"), conversation("folder")];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut columns = vec![];
        for cursor in [
            Some(peer("one")),
            Some(peer("folder")),
            Some(peer("child")),
            None,
        ] {
            app.sidebar_cursor = cursor;
            terminal
                .draw(|frame| {
                    crate::ui::draw(frame, &mut app, &mut media);
                })
                .unwrap();
            columns.push(
                (0..25)
                    .find(|x| terminal.backend().buffer()[(*x, 2)].symbol() == "o")
                    .unwrap(),
            );
        }
        assert!(columns.iter().all(|x| *x == columns[0]));
    }
    #[test]
    fn enter_sends_alt_enter_inserts_newline_and_paste_never_submits() {
        let mut app = App::new(Config::default());
        app.selected_peer = Some(peer("self"));
        app.chats.insert(
            peer("self"),
            Chat {
                draft_loaded: true,
                ..Default::default()
            },
        );
        app.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
                .is_empty()
        );
        assert!(
            app.terminal(TerminalEvent::Paste("中文\n测试".into()))
                .is_empty()
        );
        assert_eq!(app.chat().unwrap().draft.text, "\n中文\n测试");
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .as_slice(),
            [Action::Send]
        ));
        assert!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
                .is_empty()
        );
        assert_eq!(app.chat().unwrap().draft.text, "\n中文\n测试");
    }
    #[test]
    fn rendering_handles_small_terminals_and_wide_characters() {
        let mut app = App::new(Config::default());
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        let mut m = msg("1", "1");
        m.text = "中文👨‍👩‍👧‍👦\n第二行".into();
        chat.merge(vec![m]);
        app.chats.insert(peer("self"), chat);
        for (w, h) in [(120, 40), (40, 12), (8, 4), (1, 1)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
            terminal
                .draw(|f| {
                    crate::ui::draw(f, &mut app, &mut media);
                })
                .unwrap();
        }
        assert_eq!(crate::ui::wrap("中文ab", 4), vec!["中文", "ab"]);
    }
    #[tokio::test]
    async fn stale_history_response_cannot_replace_newer_request() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let paths = Paths::discover().unwrap();
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths,
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        app.chats.insert(
            peer("self"),
            Chat {
                generation: 2,
                ..Default::default()
            },
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 1,
                position: HistoryPoint::Latest,
                result: Ok(HistoryPage {
                    messages: vec![msg("old", "1")],
                    ..Default::default()
                }),
            },
        );
        assert!(app.chat().unwrap().messages.is_empty());
        app.chat_mut().unwrap().draft.text = "typing".into();
        app.chat_mut().unwrap().draft_version = 1;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Cached {
                peer: peer("self"),
                version: 0,
                messages: Ok(vec![]),
                draft: Ok(Draft {
                    text: "old".into(),
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().draft.text, "typing");
    }
    #[tokio::test]
    async fn ready_status_supplies_the_account_before_requesting_the_first_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Status(Ok(HostStatus {
                phase: "ready".into(),
                account: "a".into(),
                capabilities: vec!["history_navigation".into(), "request_context".into()],
                epoch: "host".into(),
                seq: 5,
                ..Default::default()
            })),
        );
        assert_eq!(app.account, "a");
        assert_eq!(app.epoch, "host");
        assert!(app.sync_needed);
        assert!(app.ready);
        app.ready = false;
        app.sync_needed = false;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Status(Ok(HostStatus {
                phase: "ready".into(),
                account: "a".into(),
                capabilities: vec!["history_navigation".into(), "request_context".into()],
                epoch: "host".into(),
                seq: 8,
                ..Default::default()
            })),
        );
        assert!(app.sync_needed);
        assert_eq!(app.seq, 8);
    }
    #[tokio::test]
    async fn staged_send_is_acknowledged_only_after_matching_draft_is_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "new".into();
        app.selected_peer = Some(peer("self"));
        app.chats.insert(peer("self"), Chat::default());
        app.chat_mut().unwrap().draft.text = "saved".into();
        for epoch in ["old", "new"] {
            let (proceed, receive) = oneshot::channel();
            apply_work(
                &mut app,
                &mut controller,
                &store,
                Work::Staged {
                    peer: peer("self"),
                    receipt: Receipt::default(),
                    draft: Draft::default(),
                    epoch: epoch.into(),
                    proceed,
                },
            );
            if epoch == "old" {
                assert!(receive.await.is_err());
                assert_eq!(app.chat().unwrap().draft.text, "saved");
            } else {
                assert!(receive.await.is_ok());
                assert!(app.chat().unwrap().draft.text.is_empty());
            }
        }
    }
    #[tokio::test]
    async fn a_late_cache_window_restores_the_saved_anchor_after_the_latest_native_page() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            selected: Some("300".into()),
            ..Default::default()
        };
        chat.viewport = Viewport {
            anchor: Some("300".into()),
            line: 1,
            follow: false,
        };
        chat.merge(vec![msg("1200", "1200")]);
        app.chats.insert(peer("self"), chat);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert_eq!(app.chat().unwrap().viewport.anchor.as_deref(), Some("300"));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Cached {
                peer: peer("self"),
                version: 0,
                messages: Ok((51..=550)
                    .map(|i| msg(&i.to_string(), &i.to_string()))
                    .collect()),
                draft: Ok(Draft::default()),
            },
        );
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert_eq!(app.chat().unwrap().viewport.anchor.as_deref(), Some("300"));
        assert_eq!(app.chat().unwrap().viewport.line, 1);
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("300"));
        assert_eq!(app.chat().unwrap().new_messages, 0);
    }
    #[tokio::test]
    async fn a_page_already_present_in_cache_does_not_mean_native_history_is_exhausted() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.merge(
            (1..=100)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        app.chats.insert(peer("self"), chat);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Before("60".into()),
                result: Ok(HistoryPage {
                    messages: vec![msg("59", "59"), msg("58", "58")],
                    has_before: true,
                    ..Default::default()
                }),
            },
        );
        assert!(!app.chat().unwrap().exhausted);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Before("1".into()),
                result: Ok(HistoryPage {
                    messages: vec![msg("1", "1")],
                    ..Default::default()
                }),
            },
        );
        assert!(app.chat().unwrap().exhausted);
    }
    #[test]
    fn half_page_scroll_counts_rendered_lines_including_long_messages() {
        let mut app = App::new(Config::default());
        app.focus = Focus::Messages;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            view_start: 20,
            view_height: 10,
            selected: Some("3".into()),
            ..Default::default()
        };
        chat.merge(vec![msg("1", "1"), msg("2", "2"), msg("3", "3")]);
        chat.rows = vec![
            ("1".into(), 0, 10),
            ("2".into(), 10, 20),
            ("3".into(), 20, 30),
        ];
        app.chats.insert(peer("self"), chat);
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let chat = app.chat().unwrap();
        assert_eq!(chat.viewport.anchor.as_deref(), Some("2"));
        assert_eq!(chat.viewport.line, 5);
        assert_eq!(chat.view_start, 15);
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.chat().unwrap().view_start, 20);
    }
    #[test]
    fn quick_switcher_can_find_a_conversation_inside_the_native_group_folder() {
        let mut app = App::new(Config::default());
        let mut folder = conversation("folder");
        folder.peer.chat_type = 7;
        folder.children = vec![conversation("child")];
        app.conversations = vec![folder, conversation("ordinary")];
        let found = app.switcher_items("child");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer, peer("child"));
        assert_eq!(
            app.switcher_items("")
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["folder", "child", "ordinary"]
        );
        app.conversations[0].children[0].peer.uid = "u_MixedCaseUID".into();
        for query in ["u_MixedCaseUID", "u_mixedcaseuid"] {
            let found = app.switcher_items(query);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].peer.uid, "u_MixedCaseUID");
        }
    }
    #[tokio::test]
    async fn preview_centers_and_enlarges_small_images_at_all_three_sizes() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("tiny.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 120, 210, 255]))
            .save(&file)
            .unwrap();
        let attachment = Attachment {
            element_id: "1".into(),
            kind: "image".into(),
            path: file.to_string_lossy().into(),
            ..Default::default()
        };
        let mut message = msg("1", "1");
        message.attachments = vec![
            attachment.clone(),
            Attachment {
                element_id: "2".into(),
                ..attachment.clone()
            },
        ];
        let mut app = App::new(Config::default());
        app.modal = Modal::Preview {
            message,
            attachment,
            zoom: PreviewZoom::Normal,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let mut previous = 0;
        for (zoom, percent) in [
            (PreviewZoom::Normal, 80u16),
            (PreviewZoom::Large, 90),
            (PreviewZoom::Fullscreen, 100),
        ] {
            if let Modal::Preview { zoom: current, .. } = &mut app.modal {
                *current = zoom;
            }
            let mut media = crate::media::Media::new(ratatui_image::picker::Picker::halfblocks());
            let mut work = crate::ui::DrawWork::default();
            terminal
                .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
                .unwrap();
            assert_eq!(work.images.len(), 1);
            assert!(work.images[0].upscale);
            media.set_visible(work.images.iter().cloned());
            media.request(work.images[0].clone());
            let result = media.rx.recv().await.unwrap();
            media.complete(result);
            terminal
                .draw(|f| {
                    crate::ui::draw(f, &mut app, &mut media);
                })
                .unwrap();
            let points: Vec<_> = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .enumerate()
                .filter(|(_, c)| matches!(c.bg, ratatui::style::Color::Rgb(..)))
                .map(|(i, _)| ((i % 100) as u16, (i / 100) as u16))
                .collect();
            let left = points.iter().map(|p| p.0).min().unwrap();
            let right = points.iter().map(|p| p.0).max().unwrap();
            let top = points.iter().map(|p| p.1).min().unwrap();
            let bottom = points.iter().map(|p| p.1).max().unwrap();
            let popup_h = 40 * percent / 100;
            let body_top = (40 - popup_h) / 2 + 1;
            let body_h = popup_h - 3;
            assert!((i32::from(left + right) - 99).abs() <= 1);
            assert!((i32::from(top + bottom) - i32::from(2 * body_top + body_h - 1)).abs() <= 1);
            assert!(points.len() > previous);
            previous = points.len();
        }
        app.key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Modal::Preview {
                zoom: PreviewZoom::Normal,
                ..
            }
        ));
        app.key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Modal::Preview {
                zoom: PreviewZoom::Large,
                ..
            }
        ));
        app.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(matches!(
            app.modal,
            Modal::Preview {
                zoom: PreviewZoom::Fullscreen,
                ..
            }
        ));
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(&app.modal,Modal::Preview{attachment,..} if attachment.element_id=="2"));
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(&app.modal,Modal::Preview{attachment,..} if attachment.element_id=="1"));
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn older_quotes_load_without_changing_the_window_and_support_round_trip_jumps() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let mut source = msg("100", "100");
        source.reply_to = Some("1".into());
        source.reply_seq = Some("18446744073709551613".into());
        let mut original = msg("1", "1");
        original.seq = source.reply_seq.clone().unwrap();
        original.text = "older original 中文".into();
        let expected = original.clone();
        let source_copy = source.clone();
        let listener = tokio::net::UnixListener::bind(temp.path().join("socket")).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            for step in 0..3 {
                let request: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let result = match step {
                    0 => {
                        assert_eq!(request["method"], "messages.quote");
                        assert_eq!(request["params"]["id"], "100");
                        json!({"messages":[original]})
                    }
                    1 => {
                        assert_eq!(request["method"], "messages.history");
                        assert_eq!(request["params"]["around"], "1");
                        json!({"messages":[original],"has_before":false,"has_after":true,"anchor_found":true})
                    }
                    _ => {
                        assert_eq!(request["method"], "messages.history");
                        assert!(request["params"]["around"].is_null());
                        json!({"messages":[source_copy],"has_before":true,"has_after":false,"anchor_found":true})
                    }
                };
                writer
                    .write_all(
                        format!(
                            "{}\n",
                            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&temp.path().join("socket"), events)
            .await
            .unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.connected = true;
        app.selected_peer = Some(peer("self"));
        app.focus = Focus::Messages;
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(vec![source.clone()]);
        app.chats.insert(peer("self"), chat);
        controller.quote(&mut app, source.clone(), false);
        controller.quote(&mut app, source.clone(), false);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            tokio::time::timeout(Duration::from_secs(3), work.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(app.chat().unwrap().messages.len(), 1);
        assert!(
            app.chat()
                .unwrap()
                .quote_summary("1")
                .contains("older original 中文")
        );
        assert_eq!(
            store
                .quoted_message("a".into(), peer("self"), None, source.reply_seq.clone())
                .await
                .unwrap()
                .unwrap()
                .id,
            "1"
        );
        assert!(
            store
                .quoted_message("b".into(), peer("self"), None, source.reply_seq.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
                .is_empty()
        );
        let actions = app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(&actions[..], [Action::JumpQuote(_)]));
        for action in actions {
            controller.action(&mut app, action);
        }
        apply_work(
            &mut app,
            &mut controller,
            &store,
            tokio::time::timeout(Duration::from_secs(3), work.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("1"));
        assert!(app.chat().unwrap().has_later);
        assert_eq!(app.chat().unwrap().messages.len(), 1);
        let actions = app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        for action in actions {
            controller.action(&mut app, action);
        }
        apply_work(
            &mut app,
            &mut controller,
            &store,
            tokio::time::timeout(Duration::from_secs(3), work.recv())
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("100"));
        assert!(app.chat().unwrap().viewport.follow);
        assert!(app.chat().unwrap().jump_back.is_empty());
        server.await.unwrap();
        let token = uuid::Uuid::new_v4();
        app.chat_mut()
            .unwrap()
            .quotes
            .insert("stale".into(), QuoteState::Loading(token));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Quote {
                peer: peer("self"),
                key: "stale".into(),
                token: uuid::Uuid::new_v4(),
                result: Ok(Some(expected)),
            },
        );
        assert!(
            matches!(app.chat().unwrap().quotes.get("stale"),Some(QuoteState::Loading(t)) if *t==token)
        );
    }
    #[test]
    fn visible_new_messages_clear_without_g_and_visuals_have_no_filename_labels() {
        let mut app = App::new(Config::default());
        app.terminal_focused = true;
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(vec![msg("1", "1")]);
        chat.viewport = Viewport {
            anchor: Some("1".into()),
            line: 0,
            follow: false,
        };
        let mut own = msg("2", "2");
        own.is_self = true;
        own.attachments = vec![
            Attachment {
                kind: "image".into(),
                name: "opaque-id.jpg".into(),
                ..Default::default()
            },
            Attachment {
                kind: "emoji".into(),
                name: "emoji-id.png".into(),
                ..Default::default()
            },
        ];
        chat.receive(vec![own]);
        assert_eq!(chat.new_messages, 0);
        chat.receive(vec![msg("3", "3")]);
        assert_eq!(chat.new_messages, 1);
        chat.selected = Some("2".into());
        app.chats.insert(peer("self"), chat);
        let mut media = crate::media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
        let mut work = crate::ui::DrawWork::default();
        terminal
            .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
            .unwrap();
        assert!(work.readable && work.redraw);
        assert_eq!(app.chat().unwrap().new_messages, 0);
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            screen.contains('╭')
                && screen.contains('╮')
                && screen.contains('╰')
                && screen.contains('╯')
        );
        assert!(
            !screen.contains("opaque-id.jpg")
                && !screen.contains("emoji-id.png")
                && !screen.contains("[image]")
                && !screen.contains("[emoji]")
        );
        app.chat_mut().unwrap().new_messages = 1;
        app.modal = Modal::Help;
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert_eq!(app.chat().unwrap().new_messages, 1);
        app.modal = Modal::None;
        app.chat_mut().unwrap().has_later = true;
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert_eq!(app.chat().unwrap().new_messages, 1);
    }
    #[tokio::test]
    async fn partial_images_render_at_both_edges_and_preview_open_bindings_work() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("image.png");
        image::RgbaImage::from_pixel(320, 160, image::Rgba([10, 120, 210, 255]))
            .save(&file)
            .unwrap();
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        app.focus = Focus::Messages;
        let mut picture = msg("1", "1");
        picture.attachments.push(Attachment {
            element_id: "1".into(),
            kind: "image".into(),
            path: file.to_string_lossy().into(),
            ..Default::default()
        });
        let mut chat = Chat::default();
        let mut trailing = msg("2", "2");
        trailing.text = "line\n".repeat(40);
        chat.merge(vec![picture, trailing]);
        chat.selected = Some("1".into());
        chat.viewport = Viewport {
            anchor: Some("1".into()),
            line: 0,
            follow: false,
        };
        app.chats.insert(peer("self"), chat);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();
        let mut media = crate::media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut work = crate::ui::DrawWork::default();
        terminal
            .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
            .unwrap();
        assert_eq!(work.images.len(), 1);
        media.set_visible(work.images.iter().cloned());
        media.request(work.images[0].clone());
        let result = media.rx.recv().await.unwrap();
        media.complete(result);
        terminal
            .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
            .unwrap();
        media.set_visible(work.images.iter().cloned());
        media.request(work.images[0].clone());
        if media.image(&work.images[0]).is_none() {
            let result = media.rx.recv().await.unwrap();
            media.complete(result);
        }
        for line in [0, 6] {
            app.chat_mut().unwrap().viewport.line = line;
            terminal
                .draw(|f| {
                    crate::ui::draw(f, &mut app, &mut media);
                })
                .unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .any(|c| matches!(c.bg, ratatui::style::Color::Rgb(..)))
            );
        }
        app.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::Preview { .. }));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.modal.is_open());
        let actions = app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(&actions[..],[Action::Resolve(_,a)] if a.path==file.to_string_lossy()));
        let message = app
            .chat_mut()
            .unwrap()
            .messages
            .iter_mut()
            .find(|m| m.id == "1")
            .unwrap();
        message.attachments.clear();
        message.text = "https://example.com/image".into();
        let actions = app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(&actions[..],[Action::Open(url)] if url=="https://example.com/image"));
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn failed_read_requests_retry_with_backoff_and_reject_stale_results() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("read.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            for attempt in 0..2 {
                let request: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request["method"], "messages.read");
                assert_eq!(request["params"]["account"], "a");
                let response = if attempt == 0 {
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"temporary failure"}})
                } else {
                    json!({"jsonrpc":"2.0","id":request["id"],"result":{}})
                };
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.connected = true;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(vec![msg("1", "1")]);
        chat.viewport.follow = false;
        app.chats.insert(peer("self"), chat);
        controller.read_if_visible(&mut app, true);
        assert!(app.chat().unwrap().read.pending.is_none());
        app.terminal_focused = true;
        app.chat_mut().unwrap().has_later = true;
        controller.read_if_visible(&mut app, true);
        assert!(app.chat().unwrap().read.pending.is_none());
        app.chat_mut().unwrap().has_later = false;
        controller.read_if_visible(&mut app, true);
        let first = app.chat().unwrap().read.pending.unwrap();
        controller.read_if_visible(&mut app, true);
        assert_eq!(app.chat().unwrap().read.pending, Some(first));
        let failed = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, failed);
        assert!(app.chat().unwrap().read.confirmed.is_empty());
        assert!(app.chat().unwrap().read.retry_after.is_some());
        controller.read_if_visible(&mut app, true);
        assert!(app.chat().unwrap().read.pending.is_none());
        app.chat_mut().unwrap().read.retry_after = Some(Instant::now() - Duration::from_secs(1));
        controller.read_if_visible(&mut app, false);
        assert!(app.chat().unwrap().read.pending.is_none());
        controller.read_if_visible(&mut app, true);
        let second = app.chat().unwrap().read.pending.unwrap();
        assert_ne!(first, second);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Read {
                peer: peer("self"),
                token: first,
                last_id: "1".into(),
                result: Ok(()),
            },
        );
        assert_eq!(app.chat().unwrap().read.pending, Some(second));
        let success = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, success);
        assert_eq!(app.chat().unwrap().read.confirmed, "1");
        assert!(app.chat().unwrap().read.pending.is_none());
        assert!(app.chat().unwrap().read.retry_after.is_none());
        controller.read_if_visible(&mut app, true);
        assert!(app.chat().unwrap().read.pending.is_none());
        server.await.unwrap();
    }
    #[test]
    fn only_an_uncovered_bottom_viewport_can_mark_the_chat_read() {
        let mut app = App::new(Config::default());
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.merge(
            (1..=40)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        chat.viewport = Viewport {
            anchor: Some("1".into()),
            line: 0,
            follow: false,
        };
        app.chats.insert(peer("self"), chat);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut readable = true;
        terminal
            .draw(|f| readable = crate::ui::draw(f, &mut app, &mut media).readable)
            .unwrap();
        assert!(!readable);
        app.chat_mut().unwrap().viewport.anchor = Some("40".into());
        terminal
            .draw(|f| readable = crate::ui::draw(f, &mut app, &mut media).readable)
            .unwrap();
        assert!(readable);
        app.modal = Modal::Help;
        terminal
            .draw(|f| readable = crate::ui::draw(f, &mut app, &mut media).readable)
            .unwrap();
        assert!(!readable);
    }
    #[tokio::test]
    async fn native_windows_replace_disjoint_cache_and_ignore_late_cache_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.merge(
            (1..=500)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .chain([msg("1000", "1000")])
                .collect(),
        );
        chat.viewport = Viewport {
            anchor: Some("300".into()),
            line: 1,
            follow: false,
        };
        chat.selected = Some("300".into());
        app.chats.insert(peer("self"), chat);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Around("300".into()),
                result: Ok(HistoryPage {
                    messages: (281..=320)
                        .map(|i| msg(&i.to_string(), &i.to_string()))
                        .collect(),
                    has_before: true,
                    has_after: true,
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().messages.len(), 40);
        assert_eq!(app.chat().unwrap().viewport.anchor.as_deref(), Some("300"));
        assert!(app.chat().unwrap().has_later);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Cached {
                peer: peer("self"),
                version: 0,
                messages: Ok(vec![msg("1", "1"), msg("1000", "1000")]),
                draft: Ok(Draft::default()),
            },
        );
        assert_eq!(app.chat().unwrap().messages.len(), 40);
        app.chat_mut()
            .unwrap()
            .receive(vec![msg("1001", "1001"), msg("1001", "1001")]);
        assert_eq!(app.chat().unwrap().new_messages, 1);
        assert_eq!(app.chat().unwrap().messages.last().unwrap().id, "320");
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::After("320".into()),
                result: Ok(HistoryPage {
                    messages: (321..=360)
                        .map(|i| msg(&i.to_string(), &i.to_string()))
                        .collect(),
                    has_after: true,
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().messages.len(), 80);
        assert_eq!(app.chat().unwrap().viewport.anchor.as_deref(), Some("300"));
        app.focus = Focus::Messages;
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE))
                .as_slice(),
            [Action::History(HistoryPoint::Latest)]
        ));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Latest,
                result: Ok(HistoryPage {
                    messages: (962..=1001)
                        .map(|i| msg(&i.to_string(), &i.to_string()))
                        .collect(),
                    has_before: true,
                    ..Default::default()
                }),
            },
        );
        let chat = app.chat().unwrap();
        assert_eq!(chat.messages.len(), 40);
        assert_eq!(chat.messages.last().unwrap().id, "1001");
        assert!(!chat.has_later);
        assert_eq!(chat.new_messages, 0);
    }
    #[tokio::test]
    async fn missing_native_anchors_remove_the_stale_cache_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.merge(vec![msg("1", "1")]);
        chat.viewport = Viewport {
            anchor: Some("1".into()),
            line: 1,
            follow: false,
        };
        app.chats.insert(peer("self"), chat);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Around("1".into()),
                result: Ok(HistoryPage {
                    messages: vec![msg("100", "100")],
                    anchor_found: false,
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().messages[0].id, "100");
        assert!(app.chat().unwrap().viewport.anchor.is_none());
        assert!(app.notice.contains("原阅读位置"));
    }
    #[tokio::test]
    async fn an_old_host_without_history_navigation_is_not_used_for_chat_actions() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Status(Ok(HostStatus {
                phase: "ready".into(),
                account: "a".into(),
                epoch: "host".into(),
                capabilities: vec!["history".into()],
                ..Default::default()
            })),
        );
        assert!(!app.ready);
        assert!(!app.sync_needed);
        assert!(app.notice.contains("历史定位能力"));
    }
    #[test]
    fn kitty_shifted_ascii_keys_work_for_vim_bindings_and_text_entry() {
        let mut app = App::new(Config::default());
        app.focus = Focus::Messages;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            draft_loaded: true,
            has_later: true,
            ..Default::default()
        };
        chat.merge(vec![msg("1", "1")]);
        app.chats.insert(peer("self"), chat);
        let actions = app.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT));
        assert!(matches!(
            actions.as_slice(),
            [Action::History(HistoryPoint::Latest)]
        ));
        app.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT));
        app.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SHIFT));
        app.key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.text, "AVV");
    }
    #[tokio::test]
    async fn an_around_response_cannot_replace_a_newer_reading_position() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.merge(
            (1..=600)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        chat.viewport = Viewport {
            anchor: Some("500".into()),
            line: 1,
            follow: false,
        };
        chat.selected = Some("500".into());
        app.chats.insert(peer("self"), chat);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 0,
                position: HistoryPoint::Around("300".into()),
                result: Ok(HistoryPage {
                    messages: (281..=320)
                        .map(|i| msg(&i.to_string(), &i.to_string()))
                        .collect(),
                    has_before: true,
                    has_after: true,
                    ..Default::default()
                }),
            },
        );
        let chat = app.chat().unwrap();
        assert_eq!(chat.viewport.anchor.as_deref(), Some("500"));
        assert_eq!(chat.selected.as_deref(), Some("500"));
        assert_eq!(chat.messages.len(), 600);
        assert!(!chat.loaded);
    }
    #[test]
    fn late_updates_for_old_messages_do_not_reopen_gaps_in_the_current_native_window() {
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(
            (100..=140)
                .map(|i| msg(&i.to_string(), &i.to_string()))
                .collect(),
        );
        chat.viewport.follow = false;
        chat.newest_seen = Some(msg("60", "60").order_key());
        chat.receive(vec![msg("80", "80")]);
        assert_eq!(chat.messages.len(), 41);
        assert_eq!(chat.new_messages, 0);
        chat.receive(vec![msg("141", "141"), msg("141", "141")]);
        assert_eq!(chat.messages.len(), 42);
        assert_eq!(chat.new_messages, 1);
        let mut update = msg("120", "120");
        update.text = "updated".into();
        chat.receive(vec![update]);
        assert_eq!(
            chat.messages.iter().find(|m| m.id == "120").unwrap().text,
            "updated"
        );
    }
    #[tokio::test]
    async fn offline_media_requests_can_be_retried_after_native_reconnection() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "old".into();
        app.ready = true;
        controller.resolve(
            &mut app,
            msg("1", "1"),
            Attachment {
                element_id: "image".into(),
                kind: "image".into(),
                ..Default::default()
            },
            false,
        );
        assert!(app.pending_images.is_empty());
        app.pending_images
            .insert("previous native attempt".into(), false);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Status(Ok(HostStatus {
                phase: "ready".into(),
                account: "a".into(),
                epoch: "new".into(),
                capabilities: vec!["history_navigation".into(), "request_context".into()],
                ..Default::default()
            })),
        );
        assert!(app.pending_images.is_empty());
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn repeated_history_refreshes_coalesce_while_a_query_is_pending() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("history.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (initial, checked) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), reader.read_line(&mut line))
                    .await
                    .is_err()
            );
            let _ = initial.send(());
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "messages.history");
            assert!(request["params"].get("before").is_none());
            assert!(request["params"].get("around").is_none());
            let response = json!({"jsonrpc":"2.0","id":request["id"],"result":{"messages":[msg("100","100")],"has_before":true,"has_after":false,"anchor_found":true}});
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.ready = true;
        app.selected_peer = Some(peer("self"));
        app.chats.insert(
            peer("self"),
            Chat {
                loading: true,
                generation: 7,
                ..Default::default()
            },
        );
        let folder = Peer {
            chat_type: 7,
            uid: "folder".into(),
        };
        app.selected_peer = Some(folder.clone());
        app.chats.insert(folder.clone(), Chat::default());
        controller.action(&mut app, Action::History(HistoryPoint::Latest));
        assert!(!app.chats[&folder].loading);
        app.selected_peer = Some(peer("self"));
        for _ in 0..50 {
            controller.action(&mut app, Action::History(HistoryPoint::Latest));
        }
        assert!(matches!(
            app.chat().unwrap().pending_history,
            Some(HistoryPoint::Latest)
        ));
        assert_eq!(app.chat().unwrap().generation, 7);
        checked.await.unwrap();
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("self"),
                generation: 7,
                position: HistoryPoint::Before("1".into()),
                result: Err(anyhow!("obsolete request")),
            },
        );
        assert_eq!(app.chat().unwrap().generation, 8);
        assert!(app.chat().unwrap().pending_history.is_none());
        let result = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, result);
        assert_eq!(app.chat().unwrap().messages[0].id, "100");
        assert!(!app.chat().unwrap().loading);
        server.await.unwrap();
    }
    #[tokio::test]
    async fn closing_preserves_unloaded_drafts_and_saves_only_loaded_edits() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let original = Draft {
            text: "unopened draft 中文".into(),
            cursor: 3,
            ..Default::default()
        };
        store
            .save_draft("a".into(), peer("unopened"), original.clone())
            .await
            .unwrap();
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.chats.insert(peer("unopened"), Chat::default());
        let mut edited = Chat::default();
        edited.draft.text = "edited".into();
        edited.changed();
        app.chats.insert(peer("self"), edited);
        controller.flush_drafts(&app).await.unwrap();
        assert_eq!(
            store
                .draft("a".into(), peer("unopened"))
                .await
                .unwrap()
                .text,
            original.text
        );
        assert_eq!(
            store.draft("a".into(), peer("self")).await.unwrap().text,
            "edited"
        );
        let mut reopened = App::new(Config::default());
        reopened.account = "a".into();
        reopened.chats.insert(peer("unopened"), Chat::default());
        controller.flush_drafts(&reopened).await.unwrap();
        assert_eq!(
            store
                .draft("a".into(), peer("unopened"))
                .await
                .unwrap()
                .cursor,
            3
        );
    }
    #[tokio::test]
    async fn outbox_recovery_reads_the_existing_draft_before_replacing_an_unopened_chat() {
        for existing in [
            Draft {
                text: "important original".into(),
                ..Default::default()
            },
            Draft {
                reply_to: Some("42".into()),
                ..Default::default()
            },
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = Store::open(&temp.path().join("db")).await.unwrap();
            store
                .save_draft("a".into(), peer("self"), existing.clone())
                .await
                .unwrap();
            let (tx, mut work) = mpsc::channel(16);
            let mut controller = Controller {
                clipboard: Default::default(),
                backend: None,
                store: store.clone(),
                paths: Paths::discover().unwrap(),
                tx,
                connecting: false,
                status_pending: false,
                status_again: false,
            };
            let mut app = App::new(Config::default());
            app.account = "a".into();
            controller.action(
                &mut app,
                Action::RestoreDraft(
                    Receipt {
                        account: "a".into(),
                        peer: peer("self"),
                        request_id: "failed".into(),
                        state: "failed".into(),
                        ..Default::default()
                    },
                    Draft {
                        text: "recovered".into(),
                        ..Default::default()
                    },
                ),
            );
            for _ in 0..2 {
                let result = tokio::time::timeout(Duration::from_secs(2), work.recv())
                    .await
                    .unwrap()
                    .unwrap();
                apply_work(&mut app, &mut controller, &store, result);
            }
            assert!(app.chat().unwrap().draft_loaded);
            assert_eq!(app.chat().unwrap().draft.text, existing.text);
            assert_eq!(app.chat().unwrap().draft.reply_to, existing.reply_to);
            assert!(app.notice.contains("未覆盖"));
            controller.flush_drafts(&app).await.unwrap();
            assert_eq!(
                store.draft("a".into(), peer("self")).await.unwrap().text,
                existing.text
            );
        }
    }
    #[tokio::test]
    async fn recovery_rejects_late_results_but_can_replace_a_draft_the_user_has_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.draft.text = "typing".into();
        chat.changed();
        app.chats.insert(peer("self"), chat);
        let candidate = Draft {
            text: "recovered".into(),
            ..Default::default()
        };
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::RestoreDraft {
                peer: peer("self"),
                version: 0,
                draft: candidate.clone(),
                state: "failed".into(),
                existing: Ok(Draft::default()),
            },
        );
        assert_eq!(app.chat().unwrap().draft.text, "typing");
        app.chat_mut().unwrap().draft = Draft::default();
        app.chat_mut().unwrap().changed();
        let version = app.chat().unwrap().draft_version;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::RestoreDraft {
                peer: peer("self"),
                version,
                draft: candidate,
                state: "failed".into(),
                existing: Ok(Draft {
                    text: "stale persisted draft".into(),
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.chat().unwrap().draft.text, "recovered");
        assert!(matches!(app.mode, Mode::Insert));
        controller.flush_drafts(&app).await.unwrap();
        assert_eq!(
            store.draft("a".into(), peer("self")).await.unwrap().text,
            "recovered"
        );
    }
    fn native_event(seq: u64, kind: &str, messages: Vec<Message>) -> BackendEvent {
        BackendEvent::Native(Box::new(BridgeEvent {
            account: "a".into(),
            epoch: "host".into(),
            seq,
            kind: kind.into(),
            messages,
            receipt: None,
            message_peer: None,
            message_seq: String::new(),
            notification: None,
        }))
    }
    #[tokio::test]
    async fn background_batches_are_bounded_without_dropping_operation_results() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, mut work) = mpsc::channel(128);
        let (native, mut events) = mpsc::channel(128);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx: tx.clone(),
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(vec![msg("0", "0")]);
        chat.viewport.follow = false;
        app.chats.insert(peer("self"), chat);
        for seq in 1..=100 {
            native
                .send(native_event(
                    seq,
                    "messages",
                    vec![msg(&seq.to_string(), &seq.to_string())],
                ))
                .await
                .unwrap();
        }
        let receipt = Receipt {
            account: "a".into(),
            peer: peer("self"),
            request_id: "once".into(),
            state: "confirmed".into(),
            ..Default::default()
        };
        let mut submitted = receipt.clone();
        submitted.state = "submitted".into();
        app.receipts
            .insert("once".into(), (submitted, Draft::default()));
        tx.send(Work::Receipt(Ok(receipt.clone()))).await.unwrap();
        let mut late = receipt;
        late.state = "submitted".into();
        tx.send(Work::Receipt(Ok(late))).await.unwrap();
        tx.send(Work::Error("last result".into())).await.unwrap();
        controller.drain_updates(&mut app, &mut events, &mut work);
        assert!(app.chat().unwrap().messages.len() > 1 && app.chat().unwrap().messages.len() < 101);
        assert_eq!(app.receipts["once"].0.state, "confirmed");
        assert_eq!(app.notice, "last result");
        while !events.is_empty() {
            controller.drain_updates(&mut app, &mut events, &mut work);
        }
        assert_eq!(app.chat().unwrap().messages.len(), 101);
        assert_eq!(app.chat().unwrap().new_messages, 100);
        assert_eq!(app.seq, 100);
        tokio::task::yield_now().await;
        let _ = store
            .messages("a".into(), peer("self"), None)
            .await
            .unwrap();
    }
    #[test]
    fn one_large_message_update_keeps_order_selection_and_an_exact_new_message_count() {
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        chat.merge(
            (1..=10)
                .map(|n| msg(&n.to_string(), &n.to_string()))
                .collect(),
        );
        chat.viewport.follow = false;
        chat.selected = Some("5".into());
        chat.viewport.anchor = Some("5".into());
        let mut messages: Vec<_> = (11..=1010)
            .map(|n| msg(&n.to_string(), &n.to_string()))
            .collect();
        messages.push(msg("100", "100"));
        chat.receive(messages);
        assert_eq!(chat.messages.len(), 600);
        assert_eq!(chat.new_messages, 1000);
        assert_eq!(chat.selected.as_deref(), Some("5"));
        assert_eq!(chat.viewport.anchor.as_deref(), Some("5"));
        assert_eq!(chat.messages.last().unwrap().id, "600");
        assert!(chat.has_later);
    }
    #[tokio::test]
    async fn an_event_gap_invalidates_history_without_losing_drafts_or_reading_anchors() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store,
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat {
            loaded: true,
            loading: true,
            generation: 5,
            ..Default::default()
        };
        chat.merge(vec![msg("1", "1")]);
        chat.viewport.follow = false;
        chat.viewport.anchor = Some("1".into());
        chat.draft.text = "keep this draft".into();
        chat.changed();
        app.chats.insert(peer("self"), chat);
        controller.event(&mut app, native_event(3, "recent.invalidated", vec![]));
        let chat = app.chat().unwrap();
        assert!(!chat.loaded);
        assert_eq!(chat.generation, 5);
        assert!(matches!(&chat.pending_history,Some(HistoryPoint::Around(id)) if id=="1"));
        assert_eq!(chat.draft.text, "keep this draft");
        assert_eq!(chat.viewport.anchor.as_deref(), Some("1"));
        assert!(app.sync_needed);
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn queued_history_queries_keep_their_peer_after_the_user_switches_chats() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("history.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["params"]["peer"]["uid"], "old");
            let mut message = msg("100", "100");
            message.peer = peer("old");
            let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"messages":[message],"has_before":false,"has_after":false,"anchor_found":true}});
            writer
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("new"));
        app.chats.insert(peer("new"), Chat::default());
        app.chats.insert(
            peer("old"),
            Chat {
                loading: true,
                generation: 1,
                pending_history: Some(HistoryPoint::Latest),
                ..Default::default()
            },
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::History {
                peer: peer("old"),
                generation: 1,
                position: HistoryPoint::Before("1".into()),
                result: Err(anyhow!("superseded")),
            },
        );
        let reply = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, reply);
        assert_eq!(app.selected_peer, Some(peer("new")));
        assert!(app.chats[&peer("new")].messages.is_empty());
        assert_eq!(app.chats[&peer("old")].messages[0].id, "100");
        server.await.unwrap();
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn host_status_refreshes_coalesce_during_an_epoch_change() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("status.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            for _ in 0..2 {
                let request: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request["method"], "host.status");
                let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocol_version":2,"phase":"ready","account":"a","epoch":"new","seq":100,"errors":[],"capabilities":["history_navigation","request_context"]}});
                writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(30), lines.next_line())
                    .await
                    .is_err()
            );
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "old".into();
        for seq in 1..=100 {
            let mut event = native_event(seq, "recent.invalidated", vec![]);
            if let BackendEvent::Native(ref mut e) = event {
                e.epoch = "new".into();
            }
            controller.event(&mut app, event);
        }
        assert!(controller.status_pending);
        let reply = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, reply);
        assert!(controller.status_pending);
        let reply = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, reply);
        assert!(!controller.status_pending);
        assert_eq!(app.epoch, "new");
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancelling_a_loading_dialog_discards_its_late_result_and_preserves_input_focus() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        app.mode = Mode::Insert;
        app.focus = Focus::Messages;
        let token = app.begin_dialog("读取成员");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Members {
                token,
                peer: peer("self"),
                mention: true,
                result: Ok(vec![]),
            },
        );
        assert!(matches!(app.modal, Modal::None));
        assert!(app.pending_dialog.is_none());
        assert!(matches!(app.mode, Mode::Insert));
        assert!(matches!(app.focus, Focus::Messages));
    }
    #[tokio::test]
    async fn an_old_dialog_cannot_replace_a_new_request_for_the_same_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let old = app.begin_dialog("old");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let new = app.begin_dialog("new");
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Profile {
                token: old,
                peer: peer("self"),
                result: Ok(Profile {
                    nick: "old".into(),
                    ..Default::default()
                }),
            },
        );
        assert_eq!(app.pending_dialog, Some(new));
        assert!(matches!(app.modal, Modal::Loading(_)));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Profile {
                token: new,
                peer: peer("self"),
                result: Ok(Profile {
                    nick: "new".into(),
                    ..Default::default()
                }),
            },
        );
        assert!(matches!(&app.modal,Modal::Profile(p) if p.nick=="new"));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Contacts {
                token: old,
                result: Err(anyhow!("stale failure")),
            },
        );
        assert!(matches!(&app.modal,Modal::Profile(p) if p.nick=="new"));
        assert!(!app.notice.contains("stale failure"));
        let token = app.begin_dialog("current");
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Contacts {
                token,
                result: Err(anyhow!("current failure")),
            },
        );
        assert!(matches!(app.modal, Modal::None));
        assert!(app.notice.contains("current failure"));
    }
    #[tokio::test]
    async fn reopening_the_same_search_does_not_accept_an_earlier_result() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.selected_peer = Some(peer("self"));
        let old = uuid::Uuid::new_v4();
        let new = uuid::Uuid::new_v4();
        app.modal = Modal::Search {
            query: "same".into(),
            index: 0,
            results: vec![],
        };
        app.pending_dialog = Some(new);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Search {
                token: old,
                peer: peer("self"),
                query: "same".into(),
                result: Ok(vec![msg("1", "1")]),
            },
        );
        assert!(matches!(&app.modal,Modal::Search{results,..} if results.is_empty()));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Search {
                token: new,
                peer: peer("self"),
                query: "same".into(),
                result: Ok(vec![msg("2", "2")]),
            },
        );
        assert!(
            matches!(&app.modal,Modal::Search{results,..} if results.len()==1 && results[0].id=="2")
        );
        app.begin_dialog("old account");
        app.conversations = vec![conversation("self")];
        app.apply_snapshot(Snapshot {
            unread_total: None,
            account: "b".into(),
            epoch: "host".into(),
            seq: 0,
            conversations: vec![conversation("self")],
        });
        assert!(app.pending_dialog.is_none());
        assert!(matches!(app.modal, Modal::None));
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn a_connection_closed_before_attachment_is_not_kept_as_the_live_backend() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("dead.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (events, mut changes) = mpsc::channel(4);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
        let _ = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .unwrap();
        assert!(!backend.is_alive());
        let (tx, _) = mpsc::channel(8);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: true,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Connected(Ok(backend)),
        );
        assert!(controller.backend.is_none());
        assert!(!controller.connecting);
        assert!(!app.connected);
        assert!(!app.ready);
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn an_old_connection_cannot_close_or_relabel_the_current_connection() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let listener1 = tokio::net::UnixListener::bind(temp.path().join("one.sock")).unwrap();
        let listener2 = tokio::net::UnixListener::bind(temp.path().join("two.sock")).unwrap();
        let (events, _) = mpsc::channel(8);
        let first = Backend::connect(&temp.path().join("one.sock"), events.clone())
            .await
            .unwrap();
        let (_server1, _) = listener1.accept().await.unwrap();
        let second = Backend::connect(&temp.path().join("two.sock"), events)
            .await
            .unwrap();
        let (_server2, _) = listener2.accept().await.unwrap();
        let id = second.id();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(second),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: true,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.connected = true;
        controller.event(
            &mut app,
            BackendEvent::Connection {
                id: first.id(),
                event: Box::new(BackendEvent::Disconnected("old".into())),
            },
        );
        assert_eq!(controller.backend.as_ref().unwrap().id(), id);
        assert!(app.connected);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Connection {
                id: first.id(),
                epoch: None,
                work: Box::new(Work::Status(Ok(HostStatus {
                    phase: "ready".into(),
                    account: "other".into(),
                    epoch: "old".into(),
                    capabilities: vec!["history_navigation".into(), "request_context".into()],
                    ..Default::default()
                }))),
            },
        );
        assert_eq!(app.account, "a");
        assert!(controller.status_pending);
        app.syncing = true;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Connection {
                id,
                epoch: Some("old-host".into()),
                work: Box::new(Work::Snapshot(Ok(Snapshot {
                    unread_total: None,
                    account: "a".into(),
                    epoch: "host".into(),
                    seq: 1,
                    conversations: vec![conversation("old")],
                }))),
            },
        );
        assert!(app.conversations.is_empty());
        assert!(app.syncing);
    }
    #[tokio::test]
    async fn returning_to_the_same_account_does_not_reactivate_old_cache_clipboard_or_staging_results()
     {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.apply_snapshot(Snapshot {
            account: "a".into(),
            unread_total: None,
            epoch: "host".into(),
            seq: 0,
            conversations: vec![conversation("self")],
        });
        let old = app.context;
        for account in ["b", "a"] {
            app.apply_snapshot(Snapshot {
                unread_total: None,
                account: account.into(),
                epoch: "host".into(),
                seq: 0,
                conversations: vec![conversation("self")],
            });
        }
        app.chat_mut().unwrap().draft = Draft {
            text: "current draft".into(),
            ..Default::default()
        };
        app.chat_mut().unwrap().draft_loaded = true;
        app.staging = true;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Scoped {
                scope: RequestContext {
                    account: "a".into(),
                    view: old,
                    native: None,
                },
                work: Box::new(Work::Cached {
                    peer: peer("self"),
                    version: 0,
                    messages: Ok(vec![]),
                    draft: Ok(Draft {
                        text: "old draft".into(),
                        ..Default::default()
                    }),
                }),
            },
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Scoped {
                scope: RequestContext {
                    account: "a".into(),
                    view: old,
                    native: None,
                },
                work: Box::new(Work::Clipboard {
                    generation: 0,
                    peer: peer("self"),
                    result: Ok(Clipboard::Text("old paste".into())),
                }),
            },
        );
        let (proceed, accepted) = oneshot::channel();
        let stale = Work::Scoped {
            scope: RequestContext {
                account: "a".into(),
                view: old,
                native: None,
            },
            work: Box::new(Work::Staged {
                peer: peer("self"),
                receipt: Receipt::default(),
                draft: Draft::default(),
                epoch: "host".into(),
                proceed,
            }),
        };
        assert!(stale.staging_completion());
        apply_work(&mut app, &mut controller, &store, stale);
        assert!(accepted.await.is_err());
        assert_eq!(app.chat().unwrap().draft.text, "current draft");
        assert!(app.staging);
        assert!(app.receipts.is_empty());
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Scoped {
                scope: RequestContext {
                    account: "a".into(),
                    view: old,
                    native: None,
                },
                work: Box::new(Work::StageFailed {
                    epoch: "host".into(),
                    message: "old failure".into(),
                }),
            },
        );
        assert!(app.staging);
    }
    #[test]
    fn unloaded_drafts_reject_typing_and_paste_until_the_existing_draft_is_known() {
        let mut app = App::new(Config::default());
        app.selected_peer = Some(peer("self"));
        app.chats.insert(peer("self"), Chat::default());
        app.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Normal));
        app.mode = Mode::Insert;
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.terminal(TerminalEvent::Paste("paste".into()));
        assert!(app.chat().unwrap().draft.is_empty());
        assert!(!app.chat().unwrap().draft_loaded);
        app.chat_mut().unwrap().draft = Draft {
            text: "restored".into(),
            cursor: 8,
            ..Default::default()
        };
        app.chat_mut().unwrap().draft_loaded = true;
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.text, "restoredx");
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn a_scoped_send_is_durable_and_submitted_once_after_interface_acknowledgement() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("send.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "messages.send");
            assert_eq!(request["params"]["epoch"], "host");
            assert_eq!(request["params"]["text"], "scoped send");
            let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"account":"a","peer":{"chat_type":1,"uid":"self"},"request_id":request["params"]["request_id"],"native_id":"90071992547409933","message_id":"90071992547409934","state":"confirmed"}});
            writer
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.draft.text = "scoped send".into();
        chat.changed();
        app.chats.insert(peer("self"), chat);
        controller.action(&mut app, Action::Send);
        assert!(app.staging);
        let staged = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(staged.staging_completion());
        assert_eq!(
            store.outbox("a".into()).await.unwrap()[0].1.text,
            "scoped send"
        );
        apply_work(&mut app, &mut controller, &store, staged);
        assert!(app.chat().unwrap().draft.is_empty());
        assert!(!app.staging);
        let result = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, result);
        let outbox = store.outbox("a".into()).await.unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].0.state, "confirmed");
        server.await.unwrap();
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn changing_accounts_before_staging_acknowledgement_cancels_without_native_submission() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("cancel-send.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), lines.next_line())
                    .await
                    .is_err()
            );
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.selected_peer = Some(peer("self"));
        let mut chat = Chat::default();
        chat.draft.text = "never submit".into();
        chat.changed();
        app.chats.insert(peer("self"), chat);
        controller.action(&mut app, Action::Send);
        let staged = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        for account in ["b", "a"] {
            app.apply_snapshot(Snapshot {
                unread_total: None,
                account: account.into(),
                epoch: "host".into(),
                seq: 0,
                conversations: vec![conversation("self")],
            });
        }
        app.chat_mut().unwrap().draft.text = "new draft".into();
        app.chat_mut().unwrap().changed();
        apply_work(&mut app, &mut controller, &store, staged);
        let cancelled = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, cancelled);
        assert_eq!(app.chat().unwrap().draft.text, "new draft");
        let outbox = store.outbox("a".into()).await.unwrap();
        assert_eq!(outbox[0].0.state, "failed");
        assert_eq!(outbox[0].1.text, "never submit");
        server.await.unwrap();
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn unread_origin_is_captured_before_read_and_survives_counter_clear_and_jump_back() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let socket = temp.path().join("unread.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let target = Peer {
            chat_type: 2,
            uid: "group".into(),
        };
        let server_peer = target.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            for stage in 0..3 {
                let request: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let result = if stage == 0 {
                    assert_eq!(request["method"], "conversations.unread");
                    json!({"id":"5"})
                } else {
                    assert_eq!(request["method"], "messages.history");
                    if stage == 1 {
                        assert_eq!(request["params"]["around"], "5");
                    }
                    let mut message = msg(if stage == 1 { "5" } else { "105" }, "1");
                    message.peer = server_peer.clone();
                    json!({"messages":[message],"has_before":true,"has_after":stage==1,"anchor_found":true})
                };
                writer
                    .write_all(
                        format!(
                            "{}\n",
                            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (tx, mut work) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        app.epoch = "host".into();
        app.ready = true;
        app.connected = true;
        app.terminal_focused = true;
        app.capabilities.insert("unread_anchor".into());
        app.selected_peer = Some(target.clone());
        let mut last = msg("105", "1");
        last.peer = target.clone();
        let mut chat = Chat {
            loaded: true,
            ..Default::default()
        };
        let mut earlier = msg("1", "1");
        earlier.peer = target.clone();
        let mut first_unread = msg("5", "1");
        first_unread.peer = target.clone();
        chat.merge(vec![earlier, first_unread, last]);
        chat.selected = Some("105".into());
        app.chats.insert(target.clone(), chat);
        controller.capture_unread(&mut app, &target);
        controller.read_if_visible(&mut app, true);
        assert!(app.chat().unwrap().read.pending.is_none());
        let update = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, update);
        assert!(matches!(app.chat().unwrap().unread,UnreadState::Ready(Some(ref id)) if id=="5"));
        app.conversations = vec![Conversation {
            peer: target,
            unread: "0".into(),
            ..Default::default()
        }];
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
                .is_empty()
        );
        for action in app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)) {
            controller.action(&mut app, action);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut media = crate::media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|frame| {
                let work = crate::ui::draw(frame, &mut app, &mut media);
                assert!(!work.readable);
            })
            .unwrap();
        assert!(!app.chat().unwrap().viewport.follow);
        assert_eq!(app.chat().unwrap().viewport.anchor.as_deref(), Some("5"));
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("5"));
        let update = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, update);
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("5"));
        assert!(!app.chat().unwrap().viewport.follow && app.chat().unwrap().has_later);
        for action in app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)) {
            controller.action(&mut app, action);
        }
        let update = tokio::time::timeout(Duration::from_secs(2), work.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, update);
        assert!(app.chat().unwrap().viewport.follow);
        assert_eq!(app.chat().unwrap().selected.as_deref(), Some("105"));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn unread_jump_waits_for_capture_and_rejects_previous_visit_results() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(16);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        let target = peer("group");
        app.selected_peer = Some(target.clone());
        let token = uuid::Uuid::new_v4();
        app.chats.insert(
            target.clone(),
            Chat {
                unread: UnreadState::Loading { token, jump: false },
                ..Default::default()
            },
        );
        controller.action(&mut app, Action::FirstUnread);
        assert!(matches!(
            app.chat().unwrap().unread,
            UnreadState::Loading { jump: true, .. }
        ));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::UnreadTarget {
                peer: target.clone(),
                token: uuid::Uuid::new_v4(),
                result: Ok(Some("wrong".into())),
            },
        );
        assert!(matches!(
            app.chat().unwrap().unread,
            UnreadState::Loading { .. }
        ));
        app.selected_peer = Some(peer("other"));
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::UnreadTarget {
                peer: target.clone(),
                token,
                result: Ok(Some("5".into())),
            },
        );
        assert_eq!(app.selected_peer, Some(peer("other")));
        assert!(matches!(app.chats[&target].unread,UnreadState::Ready(Some(ref id)) if id=="5"));
    }
}

#[cfg(test)]
mod requested_features_tests {
    use super::*;
    fn fixture() -> (App, Peer) {
        let peer = Peer {
            chat_type: 1,
            uid: "self".into(),
        };
        let mut app = App::new(Config::default());
        app.account = "123".into();
        app.selected_peer = Some(peer.clone());
        app.focus = Focus::Messages;
        let mut chat = Chat {
            draft_loaded: true,
            ..Default::default()
        };
        chat.merge(vec![Message {
            account: "123".into(),
            peer: peer.clone(),
            id: "1".into(),
            text: "https://example.org/a and https://example.org/b".into(),
            ..Default::default()
        }]);
        chat.selected = Some("1".into());
        app.chats.insert(peer.clone(), chat);
        (app, peer)
    }
    #[test]
    fn open_key_lists_all_links_and_attachments_and_only_opens_the_chosen_item() {
        let (mut app, _) = fixture();
        let message = &mut app.chat_mut().unwrap().messages[0];
        message.attachments.push(Attachment {
            element_id: "2".into(),
            kind: "image".into(),
            name: "one.png".into(),
            ..Default::default()
        });
        assert_eq!(message_objects(message).len(), 3);
        let actions = app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(actions.is_empty());
        assert!(matches!(&app.modal,Modal::Objects{items,..} if items.len()==3));
        let actions = app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(&actions[..],[Action::Open(url)] if url=="https://example.org/b"));
        assert!(!app.modal.is_open());
        let message = &mut app.chat_mut().unwrap().messages[0];
        message.text = "https://example.org/only".into();
        message.attachments.clear();
        assert!(matches!(
            &app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))[..],
            [Action::Open(_)]
        ));
        assert_eq!(
            message_urls("[first](https://example.org/1) https://example.org/2").len(),
            2
        );
    }
    #[test]
    fn unread_navigation_wraps_in_both_directions_in_native_order() {
        let (mut app, current) = fixture();
        let child = Peer {
            chat_type: 2,
            uid: "child".into(),
        };
        let last = Peer {
            chat_type: 1,
            uid: "last".into(),
        };
        app.conversations = vec![
            Conversation {
                peer: current.clone(),
                unread: "1".into(),
                ..Default::default()
            },
            Conversation {
                peer: Peer {
                    chat_type: 7,
                    uid: "folder".into(),
                },
                unread: "1".into(),
                children: vec![Conversation {
                    peer: child.clone(),
                    unread: "9".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            Conversation {
                peer: last.clone(),
                unread: "1".into(),
                ..Default::default()
            },
        ];
        for (start, prefix, expected) in [
            (current.clone(), ']', last.clone()),
            (last.clone(), ']', current.clone()),
            (current.clone(), '[', last.clone()),
            (last.clone(), '[', current.clone()),
            (child.clone(), ']', last),
            (child, '[', current),
        ] {
            app.selected_peer = Some(start);
            app.key(KeyEvent::new(KeyCode::Char(prefix), KeyModifiers::NONE));
            assert!(
                matches!(&app.key(KeyEvent::new(KeyCode::Char('q'),KeyModifiers::NONE))[..],[Action::Select(p)] if *p==expected)
            );
            assert!(!app.quit);
        }
        app.conversations[0].unread = "0".into();
        app.conversations[2].unread = "0".into();
        app.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
                .is_empty()
        );
        assert_eq!(app.notice, "没有符合条件的未读会话");
        assert_eq!(app.switcher_items("child").len(), 1);
        app.conversations.clear();
        app.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(
            app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
                .is_empty()
        );
    }
    #[test]
    fn insert_cursor_supports_unicode_words_and_line_or_composer_boundaries() {
        let (mut app, _) = fixture();
        app.mode = Mode::Insert;
        let text = "alpha βeta\n中文 test";
        app.chat_mut().unwrap().draft.text = text.into();
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.chat().unwrap().draft.cursor, text.find('β').unwrap());
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.chat().unwrap().draft.cursor, text.find('中').unwrap());
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.chat().unwrap().draft.cursor, text.find('β').unwrap());
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.cursor, text.find('\n').unwrap());
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.chat().unwrap().draft.cursor, 0);
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(app.chat().unwrap().draft.cursor, text.len());
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(
            app.chat().unwrap().draft.cursor,
            text.find('\n').unwrap() + 1
        );
        for (index, _) in text.char_indices() {
            assert!(text.is_char_boundary(word_boundary(text, index, true)));
            assert!(text.is_char_boundary(word_boundary(text, index, false)));
        }
    }
    #[tokio::test]
    async fn forwarded_images_share_the_renderer_and_preview_returns_to_the_record() {
        let (mut app, peer) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("image.png");
        image::RgbImage::from_pixel(20, 20, image::Rgb([255, 0, 0]))
            .save(&file)
            .unwrap();
        let message = Message {
            account: "123".into(),
            peer,
            id: "12".into(),
            forward_path: vec!["1".into()],
            attachments: vec![Attachment {
                element_id: "8".into(),
                kind: "image".into(),
                path: file.to_string_lossy().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        app.modal = Modal::Forward {
            root: app.chat().unwrap().messages[0].clone(),
            path: vec!["1".into()],
            messages: vec![message],
            index: 0,
            scroll: 0,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut images = 0;
        terminal
            .draw(|frame| {
                images = crate::ui::draw(frame, &mut app, &mut media).images.len();
            })
            .unwrap();
        assert_eq!(images, 1);
        app.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::Preview { .. }));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::Forward { .. }));
    }
    #[tokio::test]
    async fn system_notices_are_centered_gray_without_sender_header_and_reactions_support_gr() {
        let (mut app, _) = fixture();
        let message = &mut app.chat_mut().unwrap().messages[0];
        message.sender = "hidden sender".into();
        message.reaction_notice = Some("<msg><qq nm=\"Alice\"/><url msgseq=\"987\"/></msg>".into());
        assert_eq!(message.quote_key().as_deref(), Some("seq:987"));
        assert!(message.is_notice());
        app.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert!(matches!(
            &app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))[..],
            [Action::JumpQuote(_)]
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut media);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("hidden sender"));
        let index = buffer
            .content()
            .iter()
            .position(|cell| cell.symbol() == "A")
            .unwrap();
        assert!(index % 90 > 30);
        assert_eq!(
            buffer.content()[index].fg,
            ratatui::style::Color::Rgb(130, 140, 150)
        );
    }
    #[tokio::test]
    async fn a_failed_old_record_download_cannot_close_or_overwrite_the_current_preview() {
        let (mut app, peer) = fixture();
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(4);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let attachment = Attachment {
            element_id: "7".into(),
            kind: "image".into(),
            ..Default::default()
        };
        let mut current = app.chat().unwrap().messages[0].clone();
        current.forward_path = vec!["new".into()];
        current.attachments = vec![attachment.clone()];
        app.modal = Modal::Preview {
            message: current,
            attachment,
            zoom: PreviewZoom::Normal,
        };
        app.notice = "current record".into();
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::Resolved {
                account: "123".into(),
                peer,
                id: "1".into(),
                element: "7".into(),
                forward_path: vec!["old".into()],
                result: Err(anyhow!("old failure")),
                open: false,
            },
        );
        assert!(matches!(app.modal, Modal::Preview { .. }));
        assert_eq!(app.notice, "current record");
        let Modal::Preview {
            message,
            attachment,
            ..
        } = &app.modal
        else {
            panic!("preview lost")
        };
        let key = message.media_key(&attachment.element_id);
        let target = Work::Resolved {
            account: message.account.clone(),
            peer: message.peer.clone(),
            id: message.id.clone(),
            element: attachment.element_id.clone(),
            forward_path: message.forward_path.clone(),
            result: Err(anyhow!("original expired; HTTP 404")),
            open: false,
        };
        apply_work(&mut app, &mut controller, &store, target);
        assert!(matches!(app.modal, Modal::Preview { .. }));
        assert!(app.failed_images[&key].contains("HTTP 404"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let mut media = crate::media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut draw = crate::ui::DrawWork::default();
        terminal
            .draw(|frame| draw = crate::ui::draw(frame, &mut app, &mut media))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("HTTP 404"));
        assert!(
            draw.resources.is_empty(),
            "a failed preview must not retry on redraw"
        );
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!app.failed_images.contains_key(&key));
        assert!(matches!(app.modal, Modal::Preview { .. }));
    }
    #[test]
    fn conversation_actions_target_the_sidebar_cursor_including_special_sessions() {
        let (mut app, original) = fixture();
        app.ready = true;
        for kind in [1, 7, 103] {
            let target = Conversation {
                id: format!("contact-{kind}"),
                peer: Peer {
                    chat_type: kind,
                    uid: format!("target-{kind}"),
                },
                name: "Target".into(),
                ..Default::default()
            };
            app.conversations = vec![target.clone()];
            app.sidebar_cursor = Some(target.peer.clone());
            app.focus = Focus::Conversations;
            app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
            assert!(
                matches!(&app.modal,Modal::ConversationActions{conversation,..} if conversation.id==target.id)
            );
            let actions = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(matches!(&actions[..],[Action::MarkRead(c)] if c.id==target.id));
            assert_eq!(app.selected_peer.as_ref(), Some(&original));
        }
        app.focus = Focus::Messages;
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::Actions { .. }));
        app.modal = Modal::None;
        app.selected_peer = Some(Peer {
            chat_type: 103,
            uid: "mail".into(),
        });
        assert!(chat_supported(app.selected_peer.as_ref().unwrap()));
        assert!(!app.draft_editable());
    }
}

#[cfg(test)]
mod release_regressions {
    use super::*;
    fn message(id: &str) -> Message {
        Message {
            id: id.into(),
            sender_uid: "user".into(),
            sender: "name".into(),
            time: "1700000000".into(),
            peer: Peer {
                chat_type: 1,
                uid: "peer".into(),
            },
            text: "text".into(),
            ..Default::default()
        }
    }
    #[test]
    fn forward_selection_survives_drawing_and_nested_return() {
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let root = message("1");
        let mut nested = message("3");
        nested.forward = Some("<msg/>".into());
        app.modal = Modal::Forward {
            root: root.clone(),
            path: vec!["1".into()],
            messages: vec![message("2"), nested],
            index: 0,
            scroll: 0,
        };
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut media);
            })
            .unwrap();
        assert!(matches!(&app.modal, Modal::Forward { index: 1, .. }));
        let actions = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&actions[..],[Action::Forward{path,..}] if path==&["1","3"]));
        app.modal = Modal::Forward {
            root,
            path: vec!["1".into(), "3".into()],
            messages: vec![message("4")],
            index: 0,
            scroll: 0,
        };
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(&app.modal,Modal::Forward{path,index:1,..} if path==&["1"]));
    }
    #[test]
    fn group_permissions_block_all_composer_entry_points_without_destroying_the_draft() {
        let mut app = App::new(Config::default());
        let peer = Peer {
            chat_type: 2,
            uid: "group".into(),
        };
        app.selected_peer = Some(peer.clone());
        app.capabilities.insert("speech_permissions".into());
        app.chats.insert(
            peer,
            Chat {
                draft_loaded: true,
                draft: Draft {
                    text: "keep".into(),
                    ..Default::default()
                },
                permission: Some(SpeechPermission {
                    state: "blocked".into(),
                    scope: "all".into(),
                    reason: "群已开启全员禁言".into(),
                    until: None,
                    can_moderate: false,
                }),
                ..Default::default()
            },
        );
        assert!(!app.draft_editable());
        app.mode = Mode::Insert;
        assert!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_empty()
        );
        assert!(
            app.terminal(TerminalEvent::Paste("other".into()))
                .is_empty()
        );
        assert_eq!(app.chat().unwrap().draft.text, "keep");
        app.chat_mut().unwrap().permission.as_mut().unwrap().state = "allowed".into();
        assert!(app.draft_editable());
    }
    #[test]
    fn grouped_avatar_has_stable_geometry_and_narrow_terminals_omit_the_gutter() {
        let mut app = App::new(Config::default());
        let first = message("1");
        let peer = first.peer.clone();
        let mut second = message("2");
        second.time = "1700000001".into();
        app.account = "a".into();
        app.selected_peer = Some(peer.clone());
        let mut chat = Chat::default();
        chat.merge(vec![first, second]);
        app.chats.insert(peer, chat);
        let mut media = media::Media::new(ratatui_image::picker::Picker::halfblocks());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(130, 30)).unwrap();
        let mut work = crate::ui::DrawWork::default();
        terminal
            .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
            .unwrap();
        assert_eq!(work.avatars, vec!["user"]);
        let rows = app.chat().unwrap().rows.clone();
        std::rc::Rc::make_mut(&mut app.avatars).insert("user".into(), Err("missing".into()));
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut media);
            })
            .unwrap();
        assert_eq!(rows, app.chat().unwrap().rows);
        let mut narrow =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 30)).unwrap();
        narrow
            .draw(|f| work = crate::ui::draw(f, &mut app, &mut media))
            .unwrap();
        assert!(work.avatars.is_empty());
    }
}

#[cfg(test)]
mod persistence_regressions {
    use super::*;
    #[tokio::test]
    async fn repeated_failure_does_not_resurrect_an_expired_upload_banner() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(8);
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let tx = WorkSender {
            sender: tx,
            scope: RequestContext {
                account: "a".into(),
                view: app.context,
                native: None,
            },
        };
        let failed = Receipt {
            account: "a".into(),
            request_id: "id".into(),
            state: SendState::Failed,
            error: Some("upload failed".into()),
            ..Default::default()
        };
        app.receipts.insert(
            "id".into(),
            (
                Receipt {
                    state: SendState::Submitting,
                    ..failed.clone()
                },
                Draft::default(),
            ),
        );
        apply_receipt(&mut app, &store, tx.clone(), failed.clone());
        let deadline = app.notice_deadline.as_ref().unwrap().1;
        apply_receipt(&mut app, &store, tx.clone(), failed.clone());
        assert_eq!(app.notice_deadline.as_ref().unwrap().1, deadline);
        app.expire_notices(deadline + Duration::from_secs(1));
        assert!(app.failure_times.is_empty());
        assert!(app.notice.is_empty());
        apply_receipt(&mut app, &store, tx, failed);
        assert!(app.notice.is_empty());
        assert!(app.failure_times.is_empty());
        assert_eq!(app.receipts["id"].0.state, SendState::Failed);
    }
    #[tokio::test]
    async fn saved_old_version_cannot_clear_new_edits_and_failures_remain_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(8);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        let peer = Peer {
            chat_type: 1,
            uid: "p".into(),
        };
        app.chats.insert(
            peer.clone(),
            Chat {
                draft_loaded: true,
                draft_version: 2,
                draft_saving: Some(1),
                dirty_since: Some(Instant::now()),
                ..Default::default()
            },
        );
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::DraftSaved {
                peer: peer.clone(),
                version: 1,
                result: Ok(()),
            },
        );
        assert!(app.chats[&peer].dirty_since.is_some());
        assert!(app.chats[&peer].draft_saving.is_none());
        app.chats.get_mut(&peer).unwrap().draft_saving = Some(2);
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::DraftSaved {
                peer: peer.clone(),
                version: 2,
                result: Err(anyhow!("disk busy")),
            },
        );
        assert!(app.chats[&peer].dirty_since.is_some());
        assert!(app.chats[&peer].draft_saving.is_none());
    }
}

#[cfg(all(test, unix))]
mod account_handoff_tests {
    use super::*;
    #[tokio::test]
    async fn changing_accounts_saves_dirty_drafts_before_clearing_private_projection() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("host.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (events, _) = mpsc::channel(8);
        let backend = Backend::connect(&socket, events).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let status = HostStatus {
            phase: "ready".into(),
            account: "b".into(),
            epoch: "new".into(),
            capabilities: vec!["history_navigation".into(), "request_context".into()],
            ..Default::default()
        };
        let reply = status.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "host.status");
            w.write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":request["id"],"result":reply})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        });
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: Some(backend),
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let peer = Peer {
            chat_type: 1,
            uid: "p".into(),
        };
        app.chats.insert(
            peer.clone(),
            Chat {
                draft_loaded: true,
                dirty_since: Some(Instant::now()),
                draft: Draft {
                    text: "unsaved a".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        std::rc::Rc::make_mut(&mut app.avatars).insert("old".into(), Err("old cache".into()));
        apply_work(&mut app, &mut controller, &store, Work::Status(Ok(status)));
        assert_eq!(app.account, "a");
        assert!(app.pending_account.is_some());
        assert!(!app.draft_editable());
        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        apply_work(&mut app, &mut controller, &store, completion);
        assert_eq!(app.account, "b");
        assert!(app.avatars.is_empty());
        assert!(app.chats.is_empty());
        assert_eq!(
            store.draft("a".into(), peer).await.unwrap().text,
            "unsaved a"
        );
        assert!(app.media_reset);
    }
    #[tokio::test]
    async fn failed_account_save_keeps_the_old_draft_and_view_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("db")).await.unwrap();
        let (tx, _) = mpsc::channel(8);
        let mut controller = Controller {
            clipboard: Default::default(),
            backend: None,
            store: store.clone(),
            paths: Paths::discover().unwrap(),
            tx,
            connecting: false,
            status_pending: false,
            status_again: false,
        };
        let mut app = App::new(Config::default());
        app.account = "a".into();
        let peer = Peer {
            chat_type: 1,
            uid: "p".into(),
        };
        app.chats.insert(
            peer.clone(),
            Chat {
                draft_loaded: true,
                draft: Draft {
                    text: "keep".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let token = uuid::Uuid::new_v4();
        app.pending_account = Some(token);
        app.staging = true;
        apply_work(
            &mut app,
            &mut controller,
            &store,
            Work::AccountSaved {
                token,
                result: Err(anyhow!("disk full")),
            },
        );
        assert_eq!(app.account, "a");
        assert_eq!(app.chats[&peer].draft.text, "keep");
        assert!(app.pending_account.is_none());
        assert!(!app.ready);
    }
}
