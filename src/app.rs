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
pub fn find_conversation<'a>(items: &'a [Conversation], peer: &Peer) -> Option<&'a Conversation> {
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
pub fn chat_supported(peer: &Peer) -> bool {
    matches!(peer.chat_type, 1 | 2 | 8 | 100 | 103)
}
#[derive(Default)]
pub struct ReadState {
    pub confirmed: String,
    pub pending: Option<uuid::Uuid>,
    pub retry_after: Option<Instant>,
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
                if matches!(a.kind.as_str(), "image" | "sticker" | "emoji") {
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
#[derive(Clone)]
pub struct RequestContext {
    account: String,
    view: uuid::Uuid,
    native: Option<(uuid::Uuid, String)>,
}
pub enum Work {
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
        account: String,
        purpose: EmojiPurpose,
        result: Result<Vec<Sticker>>,
    },
    StickerPreview {
        token: uuid::Uuid,
        account: String,
        id: String,
        result: Result<String>,
    },
    StickerChosen {
        token: uuid::Uuid,
        account: String,
        peer: Peer,
        result: Result<Attachment>,
    },
    Reactions {
        token: uuid::Uuid,
        account: String,
        result: Result<Message>,
    },
    ReactionMembers {
        token: uuid::Uuid,
        account: String,
        message: Message,
        reaction: Reaction,
        result: Result<ReactionPage>,
    },
    ReactionRefresh {
        account: String,
        peer: Peer,
        seq: String,
        result: Result<HistoryPage>,
    },
    Forward {
        token: uuid::Uuid,
        account: String,
        root: Message,
        path: Vec<String>,
        result: Result<HistoryPage>,
    },
    Quote {
        account: String,
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
        account: String,
        peer: Peer,
        version: u64,
        messages: Result<Vec<Message>>,
        draft: Result<Draft>,
    },
    History {
        account: String,
        peer: Peer,
        generation: u64,
        position: HistoryPoint,
        result: Result<HistoryPage>,
    },
    Staged {
        account: String,
        peer: Peer,
        receipt: Receipt,
        draft: Draft,
        epoch: String,
        proceed: oneshot::Sender<()>,
    },
    RestoreDraft {
        account: String,
        peer: Peer,
        version: u64,
        draft: Draft,
        state: String,
        existing: Result<Draft>,
    },
    StageFailed {
        account: String,
        epoch: String,
        message: String,
    },
    Profile {
        token: uuid::Uuid,
        account: String,
        peer: Peer,
        result: Result<Profile>,
    },
    Recalled {
        account: String,
        peer: Peer,
        messages: Vec<Message>,
    },
    ConversationRead {
        account: String,
        epoch: String,
        result: Result<()>,
    },
    Read {
        account: String,
        peer: Peer,
        epoch: String,
        token: uuid::Uuid,
        last_id: String,
        result: Result<()>,
    },
    RecallSubmitted {
        account: String,
        peer: Peer,
        id: String,
    },
    Receipt(Result<Receipt>),
    Clipboard {
        account: String,
        peer: Peer,
        generation: u64,
        result: Result<Clipboard>,
    },
    Attachment {
        account: String,
        peer: Peer,
        generation: u64,
        result: Result<Attachment>,
    },
    Members {
        token: uuid::Uuid,
        account: String,
        peer: Peer,
        mention: bool,
        result: Result<Vec<Member>>,
    },
    Contacts {
        token: uuid::Uuid,
        account: String,
        result: Result<Vec<Conversation>>,
    },
    Search {
        token: uuid::Uuid,
        account: String,
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
    Outbox(String, Result<Vec<(Receipt, Draft)>>),
    Error(String),
    Refresh,
    Folder {
        token: uuid::Uuid,
        account: String,
        peer: Peer,
        result: Result<Vec<Conversation>>,
    },
}
#[derive(Clone)]
struct WorkSender {
    sender: mpsc::Sender<Work>,
    scope: RequestContext,
}
impl WorkSender {
    async fn send(&self, work: Work) -> Result<(), mpsc::error::SendError<()>> {
        self.sender
            .send(Work::Scoped {
                scope: self.scope.clone(),
                work: Box::new(work),
            })
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }
    async fn send_local(&self, work: Work) -> Result<(), mpsc::error::SendError<()>> {
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
    async fn receipt(&self, result: Result<Receipt>) -> Result<(), mpsc::error::SendError<()>> {
        match result {
            Ok(receipt) => self
                .sender
                .send(Work::Receipt(Ok(receipt)))
                .await
                .map_err(|_| mpsc::error::SendError(())),
            Err(error) => self.send(Work::Receipt(Err(error))).await,
        }
    }
}
impl Work {
    fn staging_completion(&self) -> bool {
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
    pub avatars: HashMap<String, Result<Avatar, String>>,
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
            avatars: HashMap::new(),
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
        if self
            .selected_peer
            .as_ref()
            .is_some_and(|p| p.chat_type == 2)
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
        if self
            .selected_peer
            .as_ref()
            .is_some_and(|p| p.chat_type == 103)
        {
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
        self.avatars.clear();
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
        self.pending_images.clear();
        self.failed_images.clear();
        self.sticker_previews.clear();
        self.sticker_errors.clear();
        self.reaction_refreshes.clear();
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
                vec![]
            }
            TerminalEvent::Mouse(m) => {
                use crossterm::event::{MouseButton, MouseEventKind};
                use ratatui::layout::Position;
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
    fn move_conversation_cursor(&mut self, delta: isize) {
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
    fn key(&mut self, mut key: KeyEvent) -> Vec<Action> {
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
            if let Some(peer) = target
                && let Some(c) = find_conversation(&self.conversations, &peer)
            {
                if !c.children.is_empty() {
                    self.modal = Modal::Contacts {
                        query: String::new(),
                        index: 0,
                        items: c.children.clone(),
                    };
                } else if peer.chat_type == 7 {
                    return vec![Action::Select(peer), Action::Folder];
                } else {
                    self.focus = Focus::Messages;
                    if self.selected_peer.as_ref() != Some(&peer) {
                        return vec![Action::Select(peer)];
                    }
                }
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
            let group_input = self
                .selected_peer
                .as_ref()
                .is_some_and(|p| p.chat_type == 2);
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
                        let skip = parent || c.peer.chat_type == 7;
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
                    .selected_peer
                    .as_ref()
                    .is_some_and(|p| p.chat_type == 103)
                {
                    self.notice = "QQ 邮箱提醒仅支持查看".into();
                    return vec![];
                }
                if self
                    .chat()
                    .and_then(|c| c.selected_message())
                    .is_some_and(|m| m.recalled)
                {
                    self.notice = "该消息已撤回".into();
                    return vec![];
                }
                if self
                    .selected_peer
                    .as_ref()
                    .is_some_and(|p| p.chat_type == 8)
                {
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
                if self
                    .selected_peer
                    .as_ref()
                    .is_some_and(|p| p.chat_type == 2)
                {
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
                            .find(|a| matches!(a.kind.as_str(), "image" | "sticker" | "emoji"))
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
    fn modal_key(&mut self, key: KeyEvent) -> Vec<Action> {
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
                    edit_picker(key, &mut query, &mut index, choices.len());
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
                        .find(|a| matches!(a.kind.as_str(), "image" | "sticker" | "emoji"))
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
                            let images: Vec<_> = message
                                .attachments
                                .iter()
                                .filter(|a| {
                                    matches!(a.kind.as_str(), "image" | "sticker" | "emoji")
                                })
                                .collect();
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

pub fn action_labels(app: &App) -> Vec<(String, bool)> {
    let message = app.chat().and_then(|c| c.selected_message());
    let selected = message.is_some();
    let no_message = (!selected).then_some("未选择消息");
    let chat_reason = no_message
        .or_else(|| message.is_some_and(|m| m.recalled).then_some("消息已撤回"))
        .or_else(|| {
            app.selected_peer
                .as_ref()
                .is_some_and(|p| p.chat_type == 8)
                .then_some("设备会话不支持")
        })
        .or_else(|| {
            app.selected_peer
                .as_ref()
                .is_some_and(|p| p.chat_type == 103)
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
                message
                    .is_some_and(|m| !m.is_self)
                    .then_some("仅自己的消息")
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
                    .is_none_or(|m| m.peer.chat_type != 2)
                    .then_some("仅群聊可用")
            }),
        ),
        label("选择表情包", (!app.ready).then_some("QQ 尚未就绪")),
    ]
}

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
    fn sender(&self, app: &App) -> WorkSender {
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
                app.pending_images.clear();
                app.failed_images.clear();
                app.sticker_previews.clear();
                app.sticker_errors.clear();
                app.reaction_refreshes.clear();
                for chat in app.chats.values_mut() {
                    chat.quotes.retain(|_, q| matches!(q, QuoteState::Ready(_)));
                    chat.quote_jump = None;
                }
                if matches!(
                    app.modal,
                    Modal::Loading(_)
                        | Modal::EmojiPicker { .. }
                        | Modal::Reactions { .. }
                        | Modal::ReactionMembers { .. }
                ) {
                    app.pending_dialog = None;
                    app.modal = Modal::None;
                }
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
                        app.avatars.clear();
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
                                    if message.seq == event.message_seq {
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
                            let tx = self.tx.clone();
                            let account = app.account.clone();
                            let seq = event.message_seq;
                            tokio::spawn(async move {
                                if let Err(e) = store.save_messages(changed).await {
                                    let _ = tx.send(Work::Error(e.to_string())).await;
                                }
                                match store.recall(account.clone(), peer.clone(), seq).await {
                                    Ok(messages) => {
                                        let _ = tx
                                            .send(Work::Recalled {
                                                account,
                                                peer,
                                                messages,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Work::Error(e.to_string())).await;
                                    }
                                }
                            });
                            app.dirty = true;
                        }
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
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = store.save_messages(messages).await {
                                let _ = tx.send(Work::Error(e.to_string())).await;
                            }
                        });
                        app.sync_needed = true;
                    }
                    "send.updated" => {
                        if let Some(receipt) = event.receipt {
                            apply_receipt(app, &self.store, &self.tx, receipt);
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
                .find(|m| Some(&m.id) == source.reply_to.as_ref())
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
        let backend = self.backend.clone().filter(|_| app.ready);
        let store = self.store.clone();
        let tx = self.sender(app);
        tokio::spawn(async move {
            let account = source.account.clone();
            let result = async {
                let cached = store
                    .quoted_message(
                        account.clone(),
                        peer.clone(),
                        source.reply_to.clone(),
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
                    account,
                    peer,
                    key,
                    token,
                    result,
                })
                .await;
        });
    }
    fn jump(&self, app: &mut App, peer: Peer, id: String) {
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
                        account,
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
        let Some(backend) = self.backend.clone().filter(|_| app.ready) else {
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
                    account,
                    id: sticker.id,
                    result,
                })
                .await;
        });
    }
    fn refresh_reactions(&self, app: &mut App, peer: Peer, seq: String) {
        if peer.chat_type != 2
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
            let result = backend
                .call(
                    "reactions.refresh",
                    json!({"account":account,"peer":peer,"seq":seq}),
                )
                .await;
            let _ = tx
                .send(Work::ReactionRefresh {
                    account,
                    peer,
                    seq,
                    result,
                })
                .await;
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
                let Some(backend) = backend.filter(|_| app.ready) else {
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
                            account,
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
                    if peer.chat_type == 8 {
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
                } else if let Some(backend) = backend.filter(|_| app.ready) {
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
                                account,
                                peer,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Reactions(message) | Action::SetReaction { message, .. }
                if message.peer.chat_type != 2 || message.recalled =>
            {
                app.notice = "仅支持对未撤回的群消息添加表情回应".into();
            }
            Action::Reactions(message) => {
                if !app.capabilities.contains("reactions") {
                    app.notice = "当前 QQ 宿主未提供表情回应接口".into();
                    return;
                }
                let Some(backend) = backend.filter(|_| app.ready) else {
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
                    let _ = tx
                        .send(Work::Reactions {
                            token,
                            account,
                            result,
                        })
                        .await;
                });
            }
            Action::SetReaction {
                message,
                reaction,
                set,
            } => {
                let Some(backend) = backend.filter(|_| app.ready) else {
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
                    let _ = tx
                        .send(Work::Reactions {
                            token,
                            account,
                            result,
                        })
                        .await;
                });
            }
            Action::ReactionMembers {
                message,
                reaction,
                cookie,
            } => {
                let Some(backend) = backend.filter(|_| app.ready) else {
                    return;
                };
                let token = app.begin_dialog("读取回应成员…");
                tokio::spawn(async move {
                    let result=backend.call("reactions.members",json!({"account":account,"peer":message.peer,"id":message.id,"emoji_id":reaction.emoji_id,"emoji_type":reaction.emoji_type,"cookie":cookie.unwrap_or_default()})).await;
                    let _ = tx
                        .send(Work::ReactionMembers {
                            token,
                            account,
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
                let Some(backend) = backend.filter(|_| app.ready) else {
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
                            account,
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
            Action::Select(peer) => {
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
                app.chats.entry(peer.clone()).or_default();
                app.chats.get_mut(&peer).unwrap().read.confirmed.clear();
                self.load_chat(app, &peer);
            }
            Action::MarkRead(conversation) => {
                if let Some(b) = backend.filter(|_| app.ready) {
                    let epoch = app.epoch.clone();
                    tokio::spawn(async move {
                        let result=b.call::<Value>("conversations.read",json!({"account":account,"epoch":epoch,"id":conversation.id,"peer":conversation.peer})).await.map(|_|());
                        let _ = tx
                            .send(Work::ConversationRead {
                                account,
                                epoch,
                                result,
                            })
                            .await;
                    });
                } else {
                    app.notice = "QQ 原生服务未就绪".into();
                }
            }
            Action::Folder => {
                if let (Some(b), Some(peer)) = (backend.filter(|_| app.ready), peer) {
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
                                account,
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
                if let (Some(b), Some(peer)) = (backend.filter(|_| app.ready), peer) {
                    let token = app.begin_dialog("读取联系人资料…");
                    tokio::spawn(async move {
                        let result = b
                            .call("profiles.info", json!({"account":account,"uid":uid}))
                            .await;
                        let _ = tx
                            .send(Work::Profile {
                                token,
                                account,
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
                            account,
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
                                    account: account.clone(),
                                    epoch: epoch.clone(),
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                        let (proceed, accepted) = oneshot::channel();
                        let _ = tx
                            .send_local(Work::Staged {
                                account: account.clone(),
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
                                account,
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
                                account,
                                peer,
                                generation,
                                result,
                            })
                            .await;
                    });
                }
            }
            Action::Members(mention) => {
                if let (Some(b), Some(peer)) = (backend.filter(|_| app.ready), peer) {
                    if peer.chat_type != 2 {
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
                                account,
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
                if let Some(b) = backend.filter(|_| app.ready) {
                    let token = app.begin_dialog("读取联系人…");
                    tokio::spawn(async move {
                        let result = b
                            .call::<Value>("contacts.list", json!({"account":account}))
                            .await
                            .and_then(parse_contacts);
                        let _ = tx
                            .send(Work::Contacts {
                                token,
                                account,
                                result,
                            })
                            .await;
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
                                account,
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
                        Ok(_)=>{let _=tx.send(Work::RecallSubmitted{account:message.account,peer:message.peer,id:message.id}).await;},
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
        let Some(b) = self.backend.clone().filter(|_| app.ready) else {
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
        if peer.chat_type != 2 || !app.capabilities.contains("speech_permissions") {
            return;
        }
        let Some(b) = self.backend.clone().filter(|_| app.ready) else {
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
        let Some(b) = self.backend.clone().filter(|_| app.ready) else {
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
                    account,
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
        let tx = self.tx.clone();
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
                    account,
                    peer,
                    epoch,
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
            if app.dirty {
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
                    Some(Ok(event))=>{if matches!(event,TerminalEvent::Resize(..)){media.invalidate();}let actions=app.terminal(event);for action in actions{controller.action(&mut app,action);}},
                    Some(Err(e))=>return Err(e.into()),None=>app.quit=true,
                },
                Some(result)=media.rx.recv()=>{media.complete(result);app.dirty=true;},
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
                    if let Some(peer)=app.selected_peer.clone() && peer.chat_type==2 && app.chat().is_some_and(|c|!c.permission_pending && (c.permission_at.is_none_or(|t|t.elapsed()>Duration::from_secs(30)) || c.permission.as_ref().is_some_and(|p|p.state=="blocked" && p.until.as_ref().and_then(|s|s.parse::<i64>().ok()).is_some_and(|t|t<=chrono::Utc::now().timestamp())))) {controller.permission(&mut app,peer);}
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
    interface_result
}
fn apply_receipt(app: &mut App, store: &Store, tx: &mpsc::Sender<Work>, receipt: Receipt) {
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
fn apply_work(app: &mut App, controller: &mut Controller, store: &Store, work: Work) {
    app.dirty = true;
    match work {
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
            if app.avatars.len() >= 256
                && let Some(id) = app.avatars.keys().next().cloned()
            {
                app.avatars.remove(&id);
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
            app.avatars.insert(uid, result.map_err(|e| e.to_string()));
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
            account,
            purpose,
            result,
        } => {
            if account == app.account && app.complete_dialog(token) {
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
        Work::StickerPreview {
            token,
            account,
            id,
            result,
        } => {
            app.sticker_previews.remove(&(token, id.clone()));
            if account == app.account
                && let Modal::EmojiPicker {
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
            account,
            peer,
            result,
        } => {
            if account == app.account {
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
        }
        Work::Reactions {
            token,
            account,
            result,
        } => {
            if account == app.account && app.complete_dialog(token) {
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
            account,
            message,
            reaction,
            result,
        } => {
            if account == app.account && app.complete_dialog(token) {
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
            account,
            peer,
            seq,
            result,
        } => {
            app.reaction_refreshes.remove(&(peer.clone(), seq));
            if account == app.account {
                match result {
                    Ok(page) => {
                        if let Some(chat) = app.chats.get_mut(&peer) {
                            chat.receive(page.messages.clone());
                        }
                        let store = store.clone();
                        tokio::spawn(async move {
                            let _ = store.save_messages(page.messages).await;
                        });
                    }
                    Err(e) => app.operation_error(&e),
                }
            }
        }

        Work::Quote {
            account,
            peer,
            key,
            token,
            result,
        } => {
            if account != app.account {
                return;
            }
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
        Work::Recalled {
            account,
            peer,
            messages,
        } => {
            if account == app.account {
                let chat = app.chats.entry(peer).or_default();
                let messages = messages
                    .into_iter()
                    .filter(|m| {
                        !chat.loaded || chat.messages.iter().any(|current| current.id == m.id)
                    })
                    .collect();
                chat.merge(messages);
                app.notice = "消息已撤回".into();
            }
        }
        Work::ConversationRead {
            account,
            epoch,
            result,
        } => {
            if app.account == account && app.epoch == epoch {
                app.sync_needed = true;
                app.notice = match result {
                    Ok(()) => "已请求标记已读，正在同步原生状态".into(),
                    Err(e) => format!("标记已读失败：{e}"),
                };
            }
        }
        Work::Read {
            account,
            peer,
            epoch,
            token,
            last_id,
            result,
        } => {
            if account == app.account
                && epoch == app.epoch
                && let Some(chat) = app.chats.get_mut(&peer)
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
        Work::RecallSubmitted { account, peer, id } => {
            if account == app.account {
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
        }

        Work::Profile {
            token,
            account,
            peer,
            result,
        } => {
            if account == app.account
                && Some(&peer) == app.selected_peer.as_ref()
                && app.complete_dialog(token)
            {
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
            account,
            peer,
            result,
        } => {
            if account == app.account
                && Some(&peer) == app.selected_peer.as_ref()
                && app.complete_dialog(token)
            {
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
                        if matches!(
                            app.modal,
                            Modal::Loading(_)
                                | Modal::EmojiPicker { .. }
                                | Modal::Reactions { .. }
                                | Modal::ReactionMembers { .. }
                        ) {
                            app.pending_dialog = None;
                            app.modal = Modal::None;
                        }
                        app.epoch = s.epoch.clone();
                        app.seq = s.seq;
                        app.pending_images.clear();
                        app.failed_images.clear();
                        app.sticker_previews.clear();
                        app.sticker_errors.clear();
                        app.reaction_refreshes.clear();
                        for chat in app.chats.values_mut() {
                            chat.quotes.retain(|_, q| matches!(q, QuoteState::Ready(_)));
                            chat.quote_jump = None;
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
                        let _ = tx.send_local(Work::Outbox(account, result)).await;
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
            account,
            peer,
            version,
            messages,
            draft,
        } => {
            if account == app.account {
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
        }
        Work::Forward {
            token,
            account,
            root,
            path,
            result,
        } => {
            if account == app.account && app.complete_dialog(token) {
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
            account,
            peer,
            generation,
            position,
            result,
        } => {
            if account == app.account {
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
                                    app.notice =
                                        "原阅读位置已不在 QQ 记录中，已加载最新历史".into();
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
        }
        Work::RestoreDraft {
            account,
            peer,
            version,
            draft,
            state,
            existing,
        } => {
            if account == app.account {
                match existing {
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
                }
            }
        }
        Work::Staged {
            account,
            peer,
            receipt,
            draft,
            epoch,
            proceed,
        } => {
            if account == app.account && epoch == app.epoch {
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
            Ok(receipt) => apply_receipt(app, store, &controller.tx, receipt),
            Err(e) => app.operation_error(&e),
        },
        Work::Clipboard {
            account,
            peer,
            generation,
            result,
        } => {
            if account == app.account {
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
        }
        Work::Attachment {
            account,
            peer,
            generation,
            result,
        } => {
            if account == app.account {
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
        }
        Work::Members {
            token,
            account,
            peer,
            mention,
            result,
        } => {
            if account == app.account
                && Some(&peer) == app.selected_peer.as_ref()
                && app.complete_dialog(token)
            {
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
        Work::Contacts {
            token,
            account,
            result,
        } => {
            if account == app.account && app.complete_dialog(token) {
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
            account,
            peer,
            query,
            result,
        } => {
            if account == app.account
                && Some(&peer) == app.selected_peer.as_ref()
                && app.complete_dialog(token)
            {
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
            if account == app.account {
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
                            && let Some(a) =
                                m.attachments.iter_mut().find(|a| a.element_id == element)
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
                            || app.modal_return.as_deref().is_some_and(|modal|matches!(modal,Modal::Forward{path,..} if path==&forward_path)) {
                            app.operation_error(&e);
                        }
                    }
                }
            }
        }
        Work::Outbox(account, result) => {
            if account == app.account {
                match result {
                    Ok(items) => {
                        for (r, d) in items {
                            app.receipts.entry(r.request_id.clone()).or_insert((r, d));
                        }
                        controller.reconcile(app);
                    }
                    Err(e) => app.operation_error(&e),
                }
            }
        }
        Work::Error(message) => {
            app.notice_deadline = Some((message.clone(), Instant::now() + Duration::from_secs(5)));
            app.notice = message;
        }
        Work::StageFailed {
            account,
            epoch,
            message,
        } => {
            if account == app.account && epoch == app.epoch {
                app.staging = false;
                app.notice_deadline =
                    Some((message.clone(), Instant::now() + Duration::from_secs(5)));
                app.notice = message;
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
    }
    #[test]
    fn native_order_updates_keep_selected_identity() {
        let mut app = App::new(Config::default());
        app.apply_snapshot(Snapshot {
            unread_total: None,
            account: "a".into(),
            epoch: "one".into(),
            seq: 10,
            conversations: vec![conversation("x"), conversation("y")],
        });
        app.selected_peer = Some(peer("y"));
        app.apply_snapshot(Snapshot {
            unread_total: None,
            account: "a".into(),
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
            unread_total: None,
            account: "a".into(),
            epoch: "one".into(),
            seq: 100,
            conversations: vec![conversation("x")],
        });
        app.apply_snapshot(Snapshot {
            unread_total: None,
            account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                capabilities: vec!["history_navigation".into(), "request_context".into()],
                account: "a".into(),
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
                capabilities: vec!["history_navigation".into(), "request_context".into()],
                account: "a".into(),
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
                    account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
                peer: peer("self"),
                epoch: "host".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
        assert_eq!(app.chat().unwrap().draft.text, "A");
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
                account: "a".into(),
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
            unread_total: None,
            account: "a".into(),
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
                    account: "a".into(),
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
                    account: "a".into(),
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
                account: "a".into(),
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
                    account: "a".into(),
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
            account: "a".into(),
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
        app.avatars.insert("user".into(), Err("missing".into()));
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
        apply_receipt(&mut app, &store, &tx, failed.clone());
        let deadline = app.notice_deadline.as_ref().unwrap().1;
        apply_receipt(&mut app, &store, &tx, failed.clone());
        assert_eq!(app.notice_deadline.as_ref().unwrap().1, deadline);
        app.expire_notices(deadline + Duration::from_secs(1));
        assert!(app.failure_times.is_empty());
        assert!(app.notice.is_empty());
        apply_receipt(&mut app, &store, &tx, failed);
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
        app.avatars.insert("old".into(), Err("old cache".into()));
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
