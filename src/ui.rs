use crate::{
    app::{App, EmojiPurpose, Focus, Modal, Mode, PreviewZoom, action_labels, sticker_choices},
    media::{ImageKey, Media},
    model::{Attachment, Message, MessagePart, Peer, Sticker, safe_text},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph,
        Wrap,
    },
};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CYAN: Color = Color::Rgb(97, 175, 190);
const MUTED: Color = Color::Rgb(130, 140, 150);
#[derive(Default)]
pub struct HitRegions {
    pub sidebar: Rect,
    pub conversations: Vec<(Rect, Peer)>,
    pub message_area: Rect,
    pub messages: Vec<(Rect, String)>,
    pub composer: Rect,
}
#[derive(Default)]
pub struct DrawWork {
    pub images: Vec<ImageKey>,
    pub avatars: Vec<String>,
    pub resources: Vec<(Message, Attachment)>,
    pub readable: bool,
    pub redraw: bool,
    pub quotes: Vec<Message>,
    pub sticker_resources: Vec<(uuid::Uuid, Sticker)>,
}
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = safe_text(text).replace('\t', "    ");
    let mut rows = vec![];
    for line in text.split('\n') {
        let mut current = String::new();
        let mut cols = 0;
        for g in line.graphemes(true) {
            let size = g.width();
            if cols + size > width && !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                cols = 0;
            }
            current.push_str(g);
            cols += size;
        }
        rows.push(current);
    }
    rows
}
struct BodyImage {
    row: usize,
    column: usize,
    width: u16,
    height: usize,
    attachment: Attachment,
}
#[derive(Default)]
struct BodyLayout {
    lines: Vec<String>,
    images: Vec<BodyImage>,
    current: String,
    column: usize,
    height: usize,
}
impl BodyLayout {
    fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
        self.lines.extend((1..self.height).map(|_| String::new()));
        self.column = 0;
        self.height = 1;
    }
    fn text(&mut self, text: &str, width: usize) {
        for g in text.graphemes(true) {
            if g == "\n" {
                self.newline();
                continue;
            }
            let cols = g.width();
            if self.column + cols > width && self.column > 0 {
                self.newline();
            }
            self.current.push_str(g);
            self.column += cols;
        }
    }
}
#[derive(Default)]
pub struct RenderCache {
    entries: std::collections::HashMap<u64, (u64, std::sync::Arc<BodyLayout>)>,
    clock: u64,
    bytes: usize,
}
fn body_layout(
    message: &Message,
    width: usize,
    media: &Media,
    cache: &mut RenderCache,
) -> std::sync::Arc<BodyLayout> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hash);
    message.account.hash(&mut hash);
    message.peer.hash(&mut hash);
    message.id.hash(&mut hash);
    message.forward_path.hash(&mut hash);
    message.revision.hash(&mut hash);
    // Local rendering inputs (such as reaction rows) have no native revision.
    if message.revision == 0 || message.id.is_empty() {
        message.text.hash(&mut hash);
        message.parts.hash(&mut hash);
        message.forward.hash(&mut hash);
        for a in &message.attachments {
            (&a.element_id, &a.kind, &a.name, &a.path, a.width, a.height).hash(&mut hash);
        }
    }
    for a in &message.attachments {
        media.inline_size(a, width).hash(&mut hash);
    }
    let key = hash.finish();
    cache.clock = cache.clock.wrapping_add(1);
    if let Some((used, body)) = cache.entries.get_mut(&key) {
        *used = cache.clock;
        return body.clone();
    }
    let body = std::sync::Arc::new(compute_body_layout(message, width, media));
    let bytes = body.lines.iter().map(String::len).sum::<usize>();
    if bytes <= 8 * 1024 * 1024 {
        while cache.entries.len() >= 1024 || cache.bytes + bytes > 8 * 1024 * 1024 {
            let Some(key) = cache
                .entries
                .iter()
                .min_by_key(|(_, v)| v.0)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some((_, old)) = cache.entries.remove(&key) {
                cache.bytes = cache
                    .bytes
                    .saturating_sub(old.lines.iter().map(String::len).sum());
            }
        }
        cache.bytes += bytes;
        cache.entries.insert(key, (cache.clock, body.clone()));
    }
    body
}
fn compute_body_layout(message: &Message, width: usize, media: &Media) -> BodyLayout {
    let width = width.max(1);
    let mut layout = BodyLayout {
        height: 1,
        ..Default::default()
    };
    for part in message.render_parts() {
        match part {
            MessagePart::Text { text } => {
                layout.text(&safe_text(&text).replace('\t', "    "), width)
            }
            MessagePart::Attachment { element_id } => {
                let Some(a) = message
                    .attachments
                    .iter()
                    .find(|a| a.element_id == element_id)
                else {
                    continue;
                };
                if a.kind == "emoji" && a.path.is_empty() {
                    layout.text(&format!("[{}]", safe_text(&a.name)), width);
                } else if a.kind == "emoji" {
                    let cols = width.min(4);
                    if layout.column + cols > width && layout.column > 0 {
                        layout.newline();
                    }
                    layout.images.push(BodyImage {
                        row: layout.lines.len(),
                        column: layout.column,
                        width: cols as u16,
                        height: 2,
                        attachment: a.clone(),
                    });
                    layout.current.push_str(&" ".repeat(cols));
                    layout.column += cols;
                    layout.height = 2;
                } else {
                    if layout.column > 0 {
                        layout.newline();
                    }
                    if matches!(a.kind.as_str(), "image" | "sticker") {
                        let (image_width, image_height) = media.inline_size(a, width);
                        layout.images.push(BodyImage {
                            row: layout.lines.len(),
                            column: 0,
                            width: image_width,
                            height: image_height,
                            attachment: a.clone(),
                        });
                        layout
                            .lines
                            .extend((0..image_height).map(|_| String::new()));
                    } else {
                        layout.text(&format!("[{}] {}", a.kind, safe_text(&a.name)), width);
                        layout.newline();
                    }
                }
            }
        }
    }
    if layout.column > 0 || !layout.current.is_empty() {
        layout.newline();
    }
    if let Some(xml) = &message.forward {
        for line in forward_summary(xml) {
            layout.text(&line, width);
            layout.newline();
        }
    }
    layout
}
pub fn forward_summary(xml: &str) -> Vec<String> {
    let mut lines = vec!["┌ 合并转发聊天记录".into()];
    if xml.len() <= 65536
        && let Ok(doc) = roxmltree::Document::parse(xml)
    {
        for node in doc
            .descendants()
            .filter(|node| {
                node.is_element() && matches!(node.tag_name().name(), "title" | "summary")
            })
            .take(6)
        {
            let text = node
                .descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>();
            let text = safe_text(&text);
            if !text.trim().is_empty() {
                lines.push(format!("│ {}", text.trim()));
            }
        }
    }
    lines.push("└ Enter 查看聊天记录".into());
    lines
}
fn panel(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { CYAN } else { Color::DarkGray }))
}
fn reaction_body(message: &Message) -> Message {
    let mut reaction_body = Message::default();
    for reaction in &message.reactions {
        let marker = if reaction.clicked { " ✓" } else { "" };
        if !reaction.path.is_empty() {
            let id = format!("reaction:{}:{}", reaction.emoji_type, reaction.emoji_id);
            reaction_body.parts.push(MessagePart::Attachment {
                element_id: id.clone(),
            });
            reaction_body.attachments.push(Attachment {
                element_id: id,
                kind: "emoji".into(),
                name: reaction.label.clone(),
                path: reaction.path.clone(),
                ..Default::default()
            });
            reaction_body.parts.push(MessagePart::Text {
                text: format!(" ×{}{marker}  ", reaction.count),
            });
        } else {
            reaction_body.parts.push(MessagePart::Text {
                text: format!("[{} ×{}{marker}] ", reaction.label, reaction.count),
            });
        }
    }
    reaction_body
}
fn grouped(previous: Option<&Message>, message: &Message) -> bool {
    let Some(p) = previous else {
        return false;
    };
    if p.is_notice()
        || message.is_notice()
        || p.sender_uid.is_empty()
        || p.sender_uid != message.sender_uid
    {
        return false;
    }
    let (Ok(a), Ok(b)) = (p.time.parse::<i64>(), message.time.parse::<i64>()) else {
        return false;
    };
    b >= a
        && b - a <= 300
        && chrono::DateTime::from_timestamp(a, 0)
            .zip(chrono::DateTime::from_timestamp(b, 0))
            .is_some_and(|(a, b)| {
                a.with_timezone(&chrono::Local).date_naive()
                    == b.with_timezone(&chrono::Local).date_naive()
            })
}
fn avatar(
    frame: &mut Frame,
    media: &mut Media,
    work: &mut DrawWork,
    paths: &std::collections::HashMap<String, Result<crate::model::Avatar, String>>,
    uid: &str,
    position: (usize, usize),
    area: Rect,
) {
    let (row, start) = position;
    if row + 2 <= start || row >= start + usize::from(area.height) {
        return;
    }
    let y = area.y + row.saturating_sub(start) as u16;
    let height = (row + 2)
        .min(start + usize::from(area.height))
        .saturating_sub(row.max(start)) as u16;
    let target = Rect::new(area.x + 1, y, 4, height);
    frame.render_widget(
        Paragraph::new(" ░░ ").style(Style::default().fg(MUTED)),
        target,
    );
    if uid.is_empty() {
        return;
    }
    work.avatars.push(uid.into());
    if let Some(Ok(value)) = paths.get(uid) {
        let key = ImageKey {
            path: value.path.clone(),
            width: 4,
            height: 2,
            upscale: true,
            animate: false,
        };
        work.images.push(key.clone());
        if let Some(protocol) = media.image(&key) {
            frame.render_widget(
                SlicedImage::new(
                    protocol,
                    SignedPosition::from((
                        0,
                        (row as i32 - start as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    )),
                ),
                Rect::new(area.x + 1, area.y, 4, area.height),
            );
        }
    }
}
pub fn draw(frame: &mut Frame, app: &mut App, media: &mut Media) -> DrawWork {
    let mut work = DrawWork::default();
    let mut hits = HitRegions::default();
    let area = frame.area();
    let [header, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let connection = if app.ready && app.connected {
        "已连接"
    } else if app.connected {
        "原生服务未就绪"
    } else {
        "离线缓存"
    };
    frame.render_widget(
        Paragraph::new(format!(" qqtui  ·  {}  ·  {connection}", app.account))
            .style(Style::default().fg(CYAN)),
        header,
    );
    let show_sidebar = app.config.sidebar_visible && body.width >= 72;
    let details_sidebar = matches!(
        app.modal,
        Modal::Members { mention: false, .. } | Modal::Profile(_)
    ) && body.width >= 110;
    let [sidebar, chat_area, details_area] = Layout::horizontal([
        Constraint::Length(if show_sidebar {
            app.config.sidebar_width.min(body.width / 3)
        } else {
            0
        }),
        Constraint::Min(1),
        Constraint::Length(if details_sidebar { 30 } else { 0 }),
    ])
    .areas(body);
    if show_sidebar {
        let items: Vec<_> = app
            .conversations
            .iter()
            .map(|c| {
                let unread = if c.unread != "0" {
                    format!("  {}", c.unread)
                } else {
                    String::new()
                };
                let pin = if c.pinned { "↑ " } else { "" };
                ListItem::new(Line::from(format!("{pin}{}{unread}", safe_text(&c.name))))
                    .style(Style::default().fg(if c.unread != "0" { Color::White } else { MUTED }))
            })
            .collect();
        let selected = app
            .sidebar_cursor
            .as_ref()
            .or(app.selected_peer.as_ref())
            .and_then(|p| app.conversations.iter().position(|c| &c.peer == p));
        let mut state = ListState::default()
            .with_selected(selected)
            .with_offset(app.sidebar_offset);
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(" 会话 ".into(), app.focus == Focus::Conversations))
                .highlight_spacing(HighlightSpacing::Always)
                .highlight_symbol("▸ ")
                .highlight_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            sidebar,
            &mut state,
        );
        app.sidebar_offset = state.offset();
        hits.sidebar = sidebar;
        let inner = Block::default().borders(Borders::ALL).inner(sidebar);
        hits.conversations = app
            .conversations
            .iter()
            .skip(state.offset())
            .take(inner.height as usize)
            .enumerate()
            .map(|(row, c)| {
                (
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                    c.peer.clone(),
                )
            })
            .collect();
    }
    let mut render_cache = std::mem::take(&mut app.render_cache);
    let avatar_paths = app.avatars.clone();
    let avatars_enabled = app.config.show_avatars;
    let speech = if app.capabilities.contains("speech_permissions")
        && app.selected_peer.as_ref().is_some_and(|p| p.chat_type == 2)
    {
        app.chat().and_then(|c| {
            c.permission.as_ref().map_or_else(
                || Some("正在确认群发言权限".into()),
                |p| (p.state != "allowed").then(|| p.description()),
            )
        })
    } else {
        None
    };
    let title = app.name();
    let focused = app.focus == Focus::Messages;
    let block = panel(format!(" {title} "), focused);
    let inner = block.inner(chat_area);
    frame.render_widget(block, chat_area);
    let bottom_binding = app.binding("bottom");
    let mode = app.mode;
    let modal_open = app.modal.is_open() && !details_sidebar;
    let pending: Vec<_> = app
        .receipts
        .values()
        .filter(|(r, _)| {
            Some(&r.peer) == app.selected_peer.as_ref()
                && r.state != "confirmed"
                && (r.state != "failed"
                    || app
                        .failure_times
                        .get(&r.request_id)
                        .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(5)))
        })
        .map(|(r, _)| {
            format!(
                "{}{}",
                match r.state.as_str() {
                    "submitting" => "提交中",
                    "submitted" => "等待确认",
                    "failed" => "失败",
                    _ => "结果未知",
                },
                r.error
                    .as_ref()
                    .map(|e| format!("：{e}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    if let Some(chat) = app.chat_mut() {
        let width = inner.width.saturating_sub(2).max(1) as usize;
        let text_rows = wrap(&chat.draft.text, width);
        let extras = usize::from(chat.draft.reply_to.is_some()) + chat.draft.attachments.len();
        let composer_h = (text_rows.len() + extras + 2)
            .max(3)
            .min((inner.height as usize / 2).max(3))
            .min(inner.height as usize) as u16;
        let banner_h =
            u16::from(!chat.viewport.follow && (chat.new_messages > 0 || chat.has_later));
        let pending_h = (pending.len().min(2)) as u16;
        let [messages_area, pending_area, banner_area, composer_area] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(pending_h),
            Constraint::Length(banner_h),
            Constraint::Length(composer_h),
        ])
        .areas(inner);
        hits.message_area = messages_area;
        hits.composer = composer_area;
        if banner_h > 0 {
            frame.render_widget(
                Paragraph::new({
                    let label = if chat.new_messages > 0 {
                        format!(" {} 条新消息", chat.new_messages)
                    } else {
                        " 历史位置 · 向下继续加载".into()
                    };
                    if focused && mode == Mode::Normal {
                        format!("{label} · {bottom_binding} 回到底部")
                    } else {
                        label
                    }
                })
                .style(Style::default().fg(Color::Yellow)),
                banner_area,
            );
        }
        if pending_h > 0 {
            frame.render_widget(
                Paragraph::new(pending.join("\n")).style(Style::default().fg(Color::Yellow)),
                pending_area,
            );
        }
        let mut lines: Vec<Line<'static>> = vec![];
        let mut placements = vec![];
        let mut quotes = vec![];
        let mut selection = None;
        let gutter = if avatars_enabled && messages_area.width >= 60 {
            6u16
        } else {
            0
        };
        let padding = " ".repeat(usize::from(gutter + 2));
        let mut avatar_rows = vec![];
        chat.rows.clear();
        let message_width = messages_area.width.saturating_sub(4 + gutter).max(1) as usize;
        let mut heights = Vec::with_capacity(chat.messages.len());
        let mut total = 0;
        for message in &chat.messages {
            let height = if message.is_notice() {
                wrap(&message.notice_text(), message_width).len() + 2
            } else {
                2 + usize::from(message.quote_key().is_some())
                    + body_layout(message, message_width, media, &mut render_cache)
                        .lines
                        .len()
                    + if message.reactions.is_empty() {
                        0
                    } else {
                        body_layout(
                            &reaction_body(message),
                            message_width,
                            media,
                            &mut render_cache,
                        )
                        .lines
                        .len()
                    }
            };
            heights.push((total, height));
            total += height;
        }
        let desired_start = if chat.viewport.follow {
            total.saturating_sub(messages_area.height as usize)
        } else {
            chat.viewport
                .anchor
                .as_ref()
                .and_then(|id| chat.messages.iter().position(|m| &m.id == id))
                .map(|i| heights[i].0 + chat.viewport.line.min(heights[i].1.saturating_sub(1)))
                .unwrap_or(0)
                .min(total.saturating_sub(messages_area.height as usize))
        };
        for (message_index, message) in chat.messages.iter().enumerate() {
            let show_header = !grouped(
                message_index
                    .checked_sub(1)
                    .and_then(|i| chat.messages.get(i)),
                message,
            );
            let start = lines.len();
            let selected = chat.selected.as_ref() == Some(&message.id);
            let height = heights[message_index].1;
            if start + height <= desired_start
                || start >= desired_start + usize::from(messages_area.height)
            {
                lines.extend((0..height).map(|_| Line::from("")));
                chat.rows.push((message.id.clone(), start, lines.len()));
                continue;
            }
            if message.is_notice() {
                lines.push(Line::from(""));
                for row in wrap(&message.notice_text(), message_width) {
                    lines.push(Line::styled(row, Style::default().fg(MUTED)).centered());
                }
                lines.push(Line::from(""));
                chat.rows.push((message.id.clone(), start, lines.len()));
                if selected {
                    selection = Some((start, lines.len(), String::new()));
                }
                continue;
            }
            let header_style = Style::default()
                .fg(if selected { CYAN } else { Color::White })
                .add_modifier(Modifier::BOLD);
            let timestamp = message
                .time
                .parse::<i64>()
                .ok()
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let title = format!(" {}  {timestamp} ", safe_text(&message.sender));
            if gutter > 0 && show_header {
                avatar_rows.push((start, message.sender_uid.clone()));
            }
            lines.push(if selected || !show_header {
                Line::from("")
            } else {
                Line::from(vec![
                    Span::styled(padding.clone(), header_style),
                    Span::styled(
                        format!("{}  {timestamp}", safe_text(&message.sender)),
                        header_style,
                    ),
                ])
            });
            if let Some(reply) = message.quote_key() {
                quotes.push((start, message.clone()));
                lines.push(Line::styled(
                    format!("{padding}↪ {}", safe_text(&chat.quote_summary(&reply))),
                    Style::default().fg(MUTED),
                ));
            }
            let body_start = lines.len();
            let layout = body_layout(message, message_width, media, &mut render_cache);
            for line in &layout.lines {
                lines.push(Line::styled(
                    format!("{padding}{line}"),
                    Style::default().fg(if selected { Color::White } else { Color::Gray }),
                ));
            }
            for image in &layout.images {
                placements.push((
                    body_start + image.row,
                    image.column,
                    image.width,
                    image.height,
                    message.clone(),
                    image.attachment.clone(),
                ));
            }
            if !message.reactions.is_empty() {
                let reaction_body = reaction_body(message);
                let offset = lines.len();
                let layout = body_layout(&reaction_body, message_width, media, &mut render_cache);
                for row in &layout.lines {
                    lines.push(Line::styled(
                        format!("{padding}{row}"),
                        Style::default().fg(CYAN),
                    ));
                }
                for image in &layout.images {
                    placements.push((
                        offset + image.row,
                        image.column,
                        image.width,
                        image.height,
                        message.clone(),
                        image.attachment.clone(),
                    ));
                }
            }
            lines.push(Line::from(""));
            chat.rows.push((message.id.clone(), start, lines.len()));
            if selected {
                selection = Some((start, lines.len(), title));
            }
        }
        chat.view_height = messages_area.height as usize;
        let max_start = lines.len().saturating_sub(chat.view_height);
        let start = if chat.viewport.follow {
            max_start
        } else {
            chat.viewport
                .anchor
                .as_ref()
                .and_then(|id| chat.rows.iter().find(|(i, _, _)| i == id))
                .map(|(_, s, e)| s + chat.viewport.line.min(e - s - 1))
                .unwrap_or(0)
                .min(max_start)
        };
        chat.view_start = start;
        hits.messages = chat
            .rows
            .iter()
            .filter_map(|(id, top, bottom)| {
                let a = (*top).max(start);
                let b = (*bottom).min(start + chat.view_height);
                (a < b).then(|| {
                    (
                        Rect::new(
                            messages_area.x,
                            messages_area.y + (a - start) as u16,
                            messages_area.width,
                            (b - a) as u16,
                        ),
                        id.clone(),
                    )
                })
            })
            .collect();
        if chat.loaded && !chat.has_later && chat.view_height > 0 && start == max_start {
            if !chat.viewport.follow {
                work.redraw = true;
            }
            chat.viewport.follow = true;
        }
        for (row, message) in quotes {
            if row + 1 >= start && row < start + chat.view_height {
                work.quotes.push(message);
            }
        }
        work.readable =
            !modal_open && chat.view_height > 0 && !chat.rows.is_empty() && start == max_start;
        if work.readable && !chat.has_later && chat.new_messages > 0 {
            chat.new_messages = 0;
            work.redraw = true;
        }
        let anchor_available = chat.viewport.follow
            || chat
                .viewport
                .anchor
                .as_ref()
                .is_none_or(|anchor| chat.rows.iter().any(|(id, _, _)| id == anchor));
        if anchor_available
            && let Some((id, s, _)) = chat.rows.iter().find(|(_, s, e)| *s <= start && start < *e)
        {
            chat.viewport.anchor = Some(id.clone());
            chat.viewport.line = start - s;
        }
        if gutter > 0 {
            for (i, (_, top, end)) in chat.rows.iter().enumerate() {
                if *end <= start || chat.messages[i].is_notice() {
                    continue;
                }
                if *top >= start + chat.view_height {
                    break;
                }
                if end.saturating_sub((*top).max(start)) < 2 {
                    continue;
                }
                if !avatar_rows
                    .iter()
                    .any(|(row, _)| row >= top && row < end && row.saturating_add(2) > start)
                {
                    avatar_rows.push(((*top).max(start), chat.messages[i].sender_uid.clone()));
                }
                break;
            }
        }
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(chat.view_height)
                    .collect::<Vec<_>>(),
            ),
            messages_area,
        );
        if !modal_open {
            for (row, uid) in avatar_rows {
                avatar(
                    frame,
                    media,
                    &mut work,
                    &avatar_paths,
                    &uid,
                    (row, start),
                    messages_area,
                );
            }
            for (row, column, width, height, message, attachment) in placements {
                if row + height <= start || row >= start + chat.view_height {
                    continue;
                }
                let rect = Rect::new(
                    messages_area.x + gutter + 2 + column as u16,
                    messages_area.y + row.saturating_sub(start) as u16,
                    width.min(
                        messages_area
                            .width
                            .saturating_sub(4 + gutter + column as u16),
                    ),
                    (row + height)
                        .min(start + chat.view_height)
                        .saturating_sub(row.max(start)) as u16,
                );
                if rect.width == 0 {
                    continue;
                }
                if attachment.path.is_empty() || !std::path::Path::new(&attachment.path).is_file() {
                    work.resources.push((message, attachment));
                    continue;
                }
                let key = ImageKey {
                    path: attachment.path.clone(),
                    width: rect.width,
                    height: height as u16,
                    upscale: false,
                    animate: true,
                };
                work.images.push(key.clone());
                if let Some(protocol) = media.image(&key) {
                    frame.render_widget(
                        SlicedImage::new(
                            protocol,
                            SignedPosition::from((
                                0,
                                (row as i32 - start as i32).clamp(i16::MIN as i32, i16::MAX as i32)
                                    as i16,
                            )),
                        ),
                        Rect::new(rect.x, messages_area.y, rect.width, messages_area.height),
                    );
                } else {
                    if let Some(error) = media.failure(&key) {
                        frame.render_widget(
                            Paragraph::new(if attachment.kind == "emoji" {
                                format!("[{}]", safe_text(&attachment.name))
                            } else {
                                format!("无法预览：{error}")
                            })
                            .style(Style::default().fg(MUTED)),
                            rect,
                        );
                    } else {
                        frame.render_widget(
                            Paragraph::new(if attachment.kind == "emoji" {
                                ""
                            } else {
                                "正在加载图片…"
                            })
                            .style(Style::default().fg(MUTED)),
                            rect,
                        );
                    }
                }
            }
        }
        if let Some((top, bottom, title)) = selection {
            let visible_top = top.max(start);
            let visible_bottom = bottom.min(start + chat.view_height);
            if visible_top < visible_bottom && messages_area.width > 1 {
                let mut borders = Borders::LEFT | Borders::RIGHT;
                if top >= start {
                    borders |= Borders::TOP;
                }
                if bottom <= start + chat.view_height {
                    borders |= Borders::BOTTOM;
                }
                let mut block = Block::default()
                    .borders(borders)
                    .border_type(BorderType::Rounded)
                    .title_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
                    .border_style(Style::default().fg(Color::Rgb(80, 200, 120)));
                if borders.contains(Borders::TOP) {
                    block = block.title(title);
                }
                frame.render_widget(
                    block,
                    Rect::new(
                        messages_area.x + gutter,
                        messages_area.y + (visible_top - start) as u16,
                        messages_area.width.saturating_sub(gutter),
                        (visible_bottom - visible_top) as u16,
                    ),
                );
            }
        }
        let composer_block = panel(
            if let Some(reason) = &speech {
                format!(" {reason} ")
            } else if !chat.draft_loaded {
                " 草稿未就绪 ".into()
            } else if mode == Mode::Insert {
                " 输入 ".into()
            } else {
                " i 开始输入 ".into()
            },
            mode == Mode::Insert && speech.is_none(),
        );
        let composer_inner = composer_block.inner(composer_area);
        frame.render_widget(composer_block, composer_area);
        let mut composer_lines = vec![];
        if let Some(id) = &chat.draft.reply_to {
            composer_lines.push(Line::styled(
                format!("↪ {}", safe_text(&chat.quote_summary(id))),
                Style::default().fg(CYAN),
            ));
        }
        for a in &chat.draft.attachments {
            composer_lines.push(Line::styled(
                format!(
                    "[{}] {}",
                    if matches!(a.kind.as_str(), "face" | "sticker") {
                        "表情"
                    } else {
                        "附件"
                    },
                    safe_text(a.display_name.as_deref().unwrap_or(&a.name))
                ),
                Style::default().fg(CYAN),
            ));
        }
        let prefix = wrap(
            &chat.draft.text[..chat.draft.cursor.min(chat.draft.text.len())],
            width,
        );
        let cursor_line = extras + prefix.len().saturating_sub(1);
        let cursor_col = prefix.last().map(|s| s.width()).unwrap_or(0);
        composer_lines.extend(text_rows.into_iter().map(Line::from));
        let scroll = cursor_line.saturating_sub(composer_inner.height.saturating_sub(1) as usize);
        frame.render_widget(
            Paragraph::new(composer_lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            composer_inner,
        );
        if mode == Mode::Insert
            && !modal_open
            && speech.is_none()
            && composer_inner.width > 0
            && composer_inner.height > 0
        {
            frame.set_cursor_position((
                composer_inner.x
                    + cursor_col.min(composer_inner.width.saturating_sub(1) as usize) as u16,
                composer_inner.y
                    + (cursor_line - scroll).min(composer_inner.height.saturating_sub(1) as usize)
                        as u16,
            ));
        }
    } else {
        frame.render_widget(
            Paragraph::new(format!(
                "\n  Ctrl+P 选择会话 · c 查看联系人\n  未登录时运行 {}",
                app.login_command()
            )),
            inner,
        );
    }
    app.render_cache = render_cache;
    let mode = if speech.is_some() {
        "READONLY"
    } else if app.mode == Mode::Insert {
        "INSERT"
    } else {
        "NORMAL"
    };
    let hint = if speech.is_some() {
        "群发言受限 · Esc 导航".into()
    } else if app.mode == Mode::Insert {
        format!(
            "{} 发送 · {} 换行 · Esc 导航",
            app.binding("send"),
            app.binding("newline")
        )
    } else {
        format!(
            "{} 输入 · {} 切换 · {} 帮助",
            app.binding("insert"),
            app.binding("switch"),
            app.binding("help")
        )
    };
    frame.render_widget(
        Paragraph::new(format!(" {mode} · {}    {hint}", safe_text(&app.notice)))
            .style(Style::default().fg(MUTED)),
        status,
    );
    if let Modal::Preview {
        message,
        attachment,
        zoom,
    } = &app.modal
    {
        let percent = match zoom {
            PreviewZoom::Normal => 80,
            PreviewZoom::Large => 90,
            PreviewZoom::Fullscreen => 100,
        };
        let width = (u32::from(area.width) * percent / 100) as u16;
        let height = (u32::from(area.height) * percent / 100) as u16;
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let images: Vec<_> = message
            .attachments
            .iter()
            .filter(|a| matches!(a.kind.as_str(), "image" | "sticker" | "emoji"))
            .collect();
        let index = images
            .iter()
            .position(|a| a.element_id == attachment.element_id)
            .unwrap_or(0)
            + 1;
        let label = match zoom {
            PreviewZoom::Normal => "默认",
            PreviewZoom::Large => "大窗",
            PreviewZoom::Fullscreen => "全屏",
        };
        let block = panel(
            format!(
                " 图片 {index}/{} · {label} · {} ",
                images.len(),
                safe_text(&attachment.name)
            ),
            true,
        );
        let inner = block.inner(rect);
        let [inner, hint] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);
        frame.render_widget(
            Paragraph::new(format!(
                "h/l 切图 · +/- 缩放 · z 全屏 · r 重试 · {} 打开 · Esc 关闭",
                app.binding("open")
            ))
            .alignment(ratatui::layout::Alignment::Center),
            hint,
        );
        let inner = if let Some(warning) = &attachment.warning {
            let [warning_area, content] =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
            frame.render_widget(
                Paragraph::new(safe_text(warning))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                warning_area,
            );
            content
        } else {
            inner
        };
        if inner.width > 0 && inner.height > 0 {
            if attachment.path.is_empty() || !std::path::Path::new(&attachment.path).is_file() {
                let key = message.media_key(&attachment.element_id);
                if let Some(error) = app.failed_images.get(&key) {
                    frame.render_widget(
                        Paragraph::new(format!(
                            "图片加载失败\n{}\n\nr 重试 · Esc 返回",
                            safe_text(error)
                        ))
                        .wrap(Wrap { trim: false })
                        .alignment(ratatui::layout::Alignment::Center),
                        inner,
                    );
                } else {
                    work.resources.push((message.clone(), attachment.clone()));
                    frame.render_widget(Paragraph::new("正在读取图片…"), inner);
                }
            } else {
                let key = ImageKey {
                    path: attachment.path.clone(),
                    width: inner.width,
                    height: inner.height,
                    upscale: true,
                    animate: true,
                };
                image_preview(frame, media, &mut work, key, inner);
            }
        }
    } else if app.modal.is_open() {
        draw_modal(
            frame,
            app,
            media,
            &mut work,
            details_sidebar.then_some(details_area),
        );
    }
    app.hit_regions = hits;
    work
}
fn image_preview(
    frame: &mut Frame,
    media: &mut Media,
    work: &mut DrawWork,
    key: ImageKey,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    work.images.push(key.clone());
    if let Some(protocol) = media.image(&key) {
        frame.render_widget(
            SlicedImage::new(
                protocol,
                SignedPosition::from((
                    (area.width.saturating_sub(protocol.size().width) / 2) as i16,
                    (area.height.saturating_sub(protocol.size().height) / 2) as i16,
                )),
            ),
            area,
        );
    } else if let Some(error) = media.failure(&key) {
        frame.render_widget(Paragraph::new(format!("无法预览：{error}")), area);
    } else {
        frame.render_widget(Paragraph::new("正在加载图片…"), area);
    }
}
fn draw_modal(
    frame: &mut Frame,
    app: &mut App,
    media: &mut Media,
    work: &mut DrawWork,
    sidebar: Option<Rect>,
) {
    let area = frame.area();
    let width = (area.width.saturating_mul(4) / 5).clamp(1, 90);
    let height = (area.height.saturating_mul(4) / 5).clamp(1, 30);
    let (width, height) = match &app.modal {
        Modal::Objects { items, .. } => (
            area.width.min(64),
            area.height.min(items.len().min(20) as u16 + 2),
        ),
        _ => (width, height),
    };
    let rect = sidebar.unwrap_or_else(|| {
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    });
    frame.render_widget(Clear, rect);
    if let Modal::EmojiPicker {
        token,
        purpose,
        items,
        query,
        index,
    } = &app.modal
    {
        let title = if matches!(purpose, EmojiPurpose::Send) {
            " 表情包 · Enter 草稿 · Ctrl+F 本地文件 · Esc 关闭 "
        } else {
            " 选择回应 · Enter 添加／取消 · Esc 关闭 "
        };
        let block = panel(title.into(), true);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let [query_area, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
        frame.render_widget(
            Paragraph::new(format!("> {}", safe_text(query))),
            query_area,
        );
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(body);
        let choices = sticker_choices(items, query);
        let mut state = ListState::default().with_selected(Some(*index));
        frame.render_stateful_widget(
            List::new(choices.iter().map(|sticker| {
                ListItem::new(format!(
                    "{} {}",
                    if sticker.kind == "sticker" {
                        "收藏"
                    } else {
                        "表情"
                    },
                    safe_text(&sticker.name)
                ))
            }))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            list,
            &mut state,
        );
        if let Some(sticker) = choices.get(*index) {
            let block = panel(" 预览 ".into(), false);
            let image_area = block.inner(preview);
            frame.render_widget(block, preview);
            if !sticker.path.is_empty() && std::path::Path::new(&sticker.path).is_file() {
                image_preview(
                    frame,
                    media,
                    work,
                    ImageKey {
                        path: sticker.path.clone(),
                        width: image_area.width,
                        height: image_area.height,
                        upscale: true,
                        animate: true,
                    },
                    image_area,
                );
            } else {
                let error = app.sticker_errors.get(&(*token, sticker.id.clone()));
                frame.render_widget(
                    Paragraph::new(error.map(String::as_str).unwrap_or(
                        if sticker.kind == "sticker" {
                            "正在读取表情…"
                        } else {
                            &sticker.name
                        },
                    )),
                    image_area,
                );
                if sticker.kind == "sticker" {
                    work.sticker_resources.push((*token, (*sticker).clone()));
                }
            }
        }
        return;
    }
    if let Modal::Forward {
        messages,
        index,
        scroll,
        ..
    } = &mut app.modal
    {
        let block = panel(
            " 合并转发 · j/k 选择 · Ctrl+u/d 滚动 · v 预览 · o 打开 · Enter 展开 · Esc 返回 "
                .into(),
            true,
        );
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let gutter = if app.config.show_avatars && inner.width >= 60 {
            6u16
        } else {
            0
        };
        let text_width = inner.width.saturating_sub(gutter).max(1) as usize;
        let prefix = " ".repeat(usize::from(gutter));
        let mut avatar_rows = vec![];
        let mut rows = Vec::new();
        let mut ranges = Vec::new();
        let mut placements = Vec::new();
        let mut notice_rows = std::collections::HashSet::new();
        for (message_index, message) in messages.iter().enumerate() {
            let show_header = !grouped(
                message_index.checked_sub(1).and_then(|i| messages.get(i)),
                message,
            );
            if message.is_notice() {
                let start = rows.len();
                for row in wrap(&message.notice_text(), inner.width as usize) {
                    notice_rows.insert(rows.len());
                    rows.push(row);
                }
                rows.push(String::new());
                ranges.push((start, rows.len()));
                continue;
            }
            let start = rows.len();
            if gutter > 0 && show_header {
                avatar_rows.push((start, message.sender_uid.clone()));
            }
            rows.push(if !show_header {
                String::new()
            } else {
                format!(
                    "{prefix}{}  {}",
                    safe_text(&message.sender),
                    message
                        .time
                        .parse::<i64>()
                        .ok()
                        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                        .map(|t| t
                            .with_timezone(&chrono::Local)
                            .format("%m-%d %H:%M")
                            .to_string())
                        .unwrap_or_default()
                )
            });
            let body = body_layout(message, text_width, media, &mut app.render_cache);
            let body_start = rows.len();
            for image in &body.images {
                placements.push((
                    body_start + image.row,
                    image.column + usize::from(gutter),
                    image.width,
                    image.height,
                    message.clone(),
                    image.attachment.clone(),
                ));
            }
            rows.extend(body.lines.iter().map(|line| format!("{prefix}{line}")));
            rows.push(String::new());
            ranges.push((start, rows.len()));
        }
        if rows.is_empty() {
            rows.push("QQ 未返回聊天记录内容".into());
        }
        *scroll = (*scroll).min(rows.len().saturating_sub(inner.height as usize));
        *index = (*index).min(messages.len().saturating_sub(1));
        if app.forward_reveal
            && let Some((a, b)) = ranges.get(*index)
        {
            if *a < *scroll {
                *scroll = *a;
            } else if *b > scroll.saturating_add(usize::from(inner.height)) {
                *scroll = b.saturating_sub(usize::from(inner.height)).min(*a);
            }
        }
        app.forward_reveal = false;
        app.forward_rows = ranges.clone();
        app.forward_area = inner;
        let selected = ranges.get(*index).copied().unwrap_or((0, 0));
        frame.render_widget(
            Paragraph::new(
                rows.into_iter()
                    .enumerate()
                    .skip(*scroll)
                    .take(inner.height as usize)
                    .map(|(i, row)| {
                        if notice_rows.contains(&i) {
                            return Line::styled(row, Style::default().fg(MUTED)).centered();
                        }
                        Line::styled(
                            row,
                            Style::default().fg(if selected.0 <= i && i < selected.1 {
                                CYAN
                            } else {
                                Color::Gray
                            }),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            inner,
        );
        for (row, uid) in avatar_rows {
            avatar(
                frame,
                media,
                work,
                &app.avatars,
                &uid,
                (row, *scroll),
                inner,
            );
        }
        for (row, column, width, height, message, attachment) in placements {
            if row + height <= *scroll || row >= scroll.saturating_add(inner.height as usize) {
                continue;
            }
            let target = Rect::new(
                inner.x + column as u16,
                inner.y,
                width.min(inner.width.saturating_sub(column as u16)),
                inner.height,
            );
            if target.width == 0 || target.height == 0 {
                continue;
            }
            if attachment.path.is_empty() || !std::path::Path::new(&attachment.path).is_file() {
                let unavailable = app
                    .failed_images
                    .contains_key(&message.media_key(&attachment.element_id));
                let y = inner.y + row.saturating_sub(*scroll) as u16;
                frame.render_widget(
                    Paragraph::new(if unavailable {
                        "图片资源暂不可用，o 可重试"
                    } else {
                        "正在读取图片…"
                    })
                    .style(Style::default().fg(MUTED)),
                    Rect::new(target.x, y, target.width, 1),
                );
                work.resources.push((message, attachment));
                continue;
            }
            let key = ImageKey {
                path: attachment.path.clone(),
                width: target.width,
                height: height as u16,
                upscale: false,
                animate: true,
            };
            work.images.push(key.clone());
            if let Some(protocol) = media.image(&key) {
                frame.render_widget(
                    SlicedImage::new(
                        protocol,
                        SignedPosition::from((
                            0,
                            (row as i32 - *scroll as i32).clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16,
                        )),
                    ),
                    target,
                );
            }
        }
        return;
    }
    let (title, query, items, index): (String, String, Vec<(String, bool)>, usize) = match &app
        .modal
    {
        Modal::Objects { items, index, .. } => (
            "打开对象 · Enter 确认 · Esc 返回".into(),
            String::new(),
            items
                .iter()
                .enumerate()
                .map(|(i, item)| (format!("{}  {}", i + 1, item.label()), true))
                .collect(),
            *index,
        ),
        Modal::Preview { .. } => return,
        Modal::EmojiPicker { .. } => return,
        Modal::Reactions { message, index } => (
            "表情回应 · Enter 成员 · Space 添加／取消 · a 新回应".into(),
            String::new(),
            if message.reactions.is_empty() {
                vec![("暂无回应，按 a 添加".into(), false)]
            } else {
                message
                    .reactions
                    .iter()
                    .map(|r| {
                        (
                            format!(
                                "{}  ×{}{}",
                                safe_text(&r.label),
                                safe_text(&r.count),
                                if r.clicked { " · 我已回应" } else { "" }
                            ),
                            true,
                        )
                    })
                    .collect()
            },
            *index,
        ),
        Modal::ReactionMembers {
            reaction,
            page,
            index,
            ..
        } => (
            format!(
                "{} 的回应成员 · {}Esc 返回",
                safe_text(&reaction.label),
                if page.cookie.is_some() {
                    "n 下一页 · "
                } else {
                    ""
                }
            ),
            String::new(),
            page.members
                .iter()
                .map(|user| {
                    (
                        format!("{} ({})", safe_text(&user.name), safe_text(&user.uid)),
                        true,
                    )
                })
                .collect(),
            *index,
        ),
        Modal::Switcher { query, index } => (
            "切换会话".into(),
            query.clone(),
            app.switcher_items(query)
                .into_iter()
                .map(|c| {
                    (
                        format!(
                            "{}  {}",
                            c.name,
                            if c.unread != "0" { &c.unread } else { "" }
                        ),
                        true,
                    )
                })
                .collect(),
            *index,
        ),
        Modal::Forward { .. } => return,
        Modal::Search {
            query,
            index,
            results,
        } => (
            "搜索缓存消息 · Enter 跳转".into(),
            query.clone(),
            results
                .iter()
                .map(|m| (format!("{}：{}", m.sender, m.text.replace('\n', " ")), true))
                .collect(),
            *index,
        ),
        Modal::Loading(label) => (
            "读取中 · Esc 取消".into(),
            String::new(),
            vec![(label.clone(), true)],
            0,
        ),
        Modal::Help => {
            let names = [
                ("stickers", "选择表情包"),
                ("reactions", "查看／添加群消息表情回应"),
                ("switch", "切换会话"),
                ("next_unread", "下一个未读会话（循环）"),
                ("previous_unread", "上一个未读会话（循环）"),
                ("insert", "进入输入模式"),
                ("send", "发送消息"),
                ("newline", "输入模式换行"),
                ("reply", "回复选中消息"),
                ("copy", "复制正文"),
                ("actions", "消息动作"),
                ("preview", "预览图片"),
                ("open", "打开图片／文件／链接"),
                ("quote", "跳转到引用原消息"),
                ("back", "返回跳转前的阅读位置"),
                ("members", "群成员"),
                ("profile", "会话资料"),
                ("outbox", "发送记录"),
                ("contacts", "联系人"),
                ("attach", "添加文件"),
                ("sticker_file", "添加本地图片表情"),
                ("clear", "清空正文、附件和回复"),
                ("sidebar", "隐藏／显示会话栏"),
                ("narrower", "缩窄会话栏"),
                ("wider", "加宽会话栏"),
                ("quit", "退出 TUI，QQ 保持在线"),
            ];
            (
                "帮助 · Esc 关闭".into(),
                String::new(),
                names
                    .into_iter()
                    .map(|(key, label)| (format!("{:<14} {label}", app.binding(key)), true))
                    .collect(),
                0,
            )
        }
        Modal::ConversationActions {
            conversation,
            index,
        } => (
            format!("会话动作 · {}", safe_text(&conversation.name)),
            String::new(),
            vec![(
                if conversation.peer.chat_type == 7 {
                    "整个分组标为已读".into()
                } else {
                    "标为已读".into()
                },
                app.ready,
            )],
            *index,
        ),
        Modal::Actions { index } => ("消息动作".into(), String::new(), action_labels(app), *index),
        Modal::AttachmentPath { input, sticker } => (
            if *sticker {
                "添加图片表情 · 输入绝对路径后 Enter"
            } else {
                "添加附件 · 输入绝对路径后 Enter"
            }
            .into(),
            input.clone(),
            vec![],
            0,
        ),
        Modal::Attachments { message, index } => (
            "附件 · Enter 用系统程序打开".into(),
            String::new(),
            message
                .attachments
                .iter()
                .map(|a| (format!("{}  {} B", a.name, a.size), true))
                .collect(),
            *index,
        ),
        Modal::Members {
            query,
            index,
            members,
            mention,
        } => (
            if *mention {
                "选择 @ 成员"
            } else {
                "群成员"
            }
            .into(),
            query.clone(),
            members
                .iter()
                .filter(|m| {
                    m.name.to_lowercase().contains(&query.to_lowercase()) || m.uin.contains(query)
                })
                .map(|m| (format!("{}  {}", m.name, m.uin), true))
                .collect(),
            *index,
        ),
        Modal::Contacts {
            query,
            index,
            items,
        } => (
            "联系人 · Enter 打开会话".into(),
            query.clone(),
            items
                .iter()
                .filter(|c| c.name.to_lowercase().contains(&query.to_lowercase()))
                .map(|c| {
                    (
                        format!(
                            "{} {}",
                            if c.peer.chat_type == 2 {
                                "群"
                            } else {
                                "好友"
                            },
                            c.name
                        ),
                        true,
                    )
                })
                .collect(),
            *index,
        ),
        Modal::ConfirmRecall(m) => (
            "撤回自己的消息 · Enter 确认 · Esc 取消".into(),
            String::new(),
            vec![(m.text.clone(), true)],
            0,
        ),
        Modal::Profile(profile) => (
            "联系人资料 · Esc 关闭".into(),
            String::new(),
            vec![
                (format!("昵称：{}", profile.nick), true),
                (format!("QQ：{}", profile.uin), true),
                (format!("备注：{}", profile.remark), true),
                (format!("签名：{}", profile.signature), true),
            ],
            0,
        ),
        Modal::None => return,
        Modal::Outbox { index, items } => (
            "发送记录 · Enter 恢复草稿（结果未知时重发可能重复）".into(),
            String::new(),
            items
                .iter()
                .map(|(r, d)| {
                    (
                        format!(
                            "[{}] {} · {}",
                            r.state,
                            r.peer.key(),
                            d.text.replace('\n', " ")
                        ),
                        true,
                    )
                })
                .collect(),
            *index,
        ),
    };
    let block = panel(format!(" {title} "), true);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let input_height = if matches!(app.modal, Modal::Objects { .. }) {
        0
    } else {
        2
    };
    let [input, list] =
        Layout::vertical([Constraint::Length(input_height), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(format!("> {}", safe_text(&query))).style(Style::default().fg(CYAN)),
        input,
    );
    let items = items.into_iter().map(|(s, enabled)| {
        ListItem::new(safe_text(&s)).style(Style::default().fg(if enabled {
            Color::White
        } else {
            MUTED
        }))
    });
    let mut state = ListState::default().with_selected(Some(index));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        list,
        &mut state,
    );
}

#[cfg(test)]
mod rich_message_tests {
    use super::*;

    #[test]
    fn ordered_faces_flow_inline_once_and_legacy_cache_does_not_guess_positions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("face.png");
        std::fs::write(&path, "asset").unwrap();
        let attachment = Attachment {
            element_id: "face".into(),
            kind: "emoji".into(),
            name: "/大冤种".into(),
            path: path.to_string_lossy().into(),
            ..Default::default()
        };
        let mut message = Message {
            text: "前[/大冤种]后".into(),
            attachments: vec![attachment],
            ..Default::default()
        };
        let layout = body_layout(
            &message,
            80,
            &Media::new(ratatui_image::picker::Picker::halfblocks()),
            &mut RenderCache::default(),
        );
        assert_eq!(layout.lines, vec!["前[/大冤种]后"]);
        assert!(layout.images.is_empty());
        message.parts = vec![
            MessagePart::Text { text: "前".into() },
            MessagePart::Attachment {
                element_id: "face".into(),
            },
            MessagePart::Text { text: "后".into() },
        ];
        let layout = body_layout(
            &message,
            80,
            &Media::new(ratatui_image::picker::Picker::halfblocks()),
            &mut RenderCache::default(),
        );
        assert_eq!(layout.lines, vec!["前    后", ""]);
        assert_eq!(layout.images.len(), 1);
        let wrapped = body_layout(
            &message,
            5,
            &Media::new(ratatui_image::picker::Picker::halfblocks()),
            &mut RenderCache::default(),
        );
        assert_eq!(wrapped.images[0].row, 1);
        message.text = "[/大冤种]".into();
        message.parts = vec![MessagePart::Attachment {
            element_id: "face".into(),
        }];
        assert_eq!(
            body_layout(
                &message,
                80,
                &Media::new(ratatui_image::picker::Picker::halfblocks()),
                &mut RenderCache::default()
            )
            .lines
            .len(),
            2
        );
        message.attachments[0].path.clear();
        let fallback = body_layout(
            &message,
            80,
            &Media::new(ratatui_image::picker::Picker::halfblocks()),
            &mut RenderCache::default(),
        );
        assert!(fallback.images.is_empty());
        assert_eq!(fallback.lines, vec!["[/大冤种]"]);
    }
    #[test]
    fn merged_card_parses_xml_entities_and_cdata_without_exposing_markup() {
        let xml = "<msg><item><title>群聊的聊天记录</title><title><![CDATA[A: <hello>]]></title><title>B: &amp; goodbye</title></item><summary>查看2条转发消息</summary></msg>";
        let lines = forward_summary(xml).join("\n");
        assert!(
            lines.contains("A: <hello>")
                && lines.contains("B: & goodbye")
                && lines.contains("查看2条转发消息")
        );
        assert!(!lines.contains("<title>") && !lines.contains("CDATA"));
        assert_eq!(
            forward_summary("<invalid"),
            vec!["┌ 合并转发聊天记录", "└ Enter 查看聊天记录"]
        );
        let message = Message {
            forward: Some(xml.into()),
            ..Default::default()
        };
        let layout = body_layout(
            &message,
            80,
            &Media::new(ratatui_image::picker::Picker::halfblocks()),
            &mut RenderCache::default(),
        );
        assert!(layout.lines.iter().any(|line| line.contains("合并转发")));
    }
}
