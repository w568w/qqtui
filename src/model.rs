use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Deserialize)]
pub struct ResolvedMedia {
    pub path: String,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct Peer {
    pub chat_type: u32,
    pub uid: String,
}
impl Peer {
    pub fn key(&self) -> String {
        format!("{}:{}", self.chat_type, self.uid)
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Conversation {
    pub peer: Peer,
    pub id: String,
    pub name: String,
    pub unread: String,
    pub pinned: bool,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub last_id: String,
    #[serde(default)]
    pub children: Vec<Conversation>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Attachment {
    #[serde(skip)]
    pub lease: Option<std::sync::Arc<crate::media::CacheLease>>,
    #[serde(default)]
    pub element_id: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub path: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Reaction {
    pub emoji_id: String,
    pub emoji_type: String,
    pub count: String,
    pub clicked: bool,
    pub label: String,
    #[serde(default)]
    pub path: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Sticker {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub path: String,
    pub emoji_id: String,
    pub emoji_type: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ReactionUser {
    pub uid: String,
    pub name: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ReactionPage {
    pub members: Vec<ReactionUser>,
    pub cookie: Option<String>,
}
pub struct ReactionNotice {
    pub seq: String,
    pub actor: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, Hash)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    Attachment { element_id: String },
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Message {
    #[serde(skip, default = "next_message_revision")]
    pub revision: u64,
    pub account: String,
    pub peer: Peer,
    pub id: String,
    #[serde(default)]
    pub seq: String,
    #[serde(default)]
    pub time: String,
    pub sender: String,
    pub sender_uid: String,
    #[serde(default)]
    pub is_self: bool,
    pub text: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
    #[serde(default)]
    pub forward: Option<String>,
    #[serde(default)]
    pub forward_path: Vec<String>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub reaction_notice: Option<String>,
    #[serde(default)]
    pub system_notice: bool,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub reply_seq: Option<String>,
    #[serde(default)]
    pub send_status: i32,
    #[serde(default)]
    pub correlation: String,
    #[serde(default)]
    pub recalled: bool,
}
pub fn next_message_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
impl Message {
    pub fn touch(&mut self) {
        self.revision = next_message_revision();
    }
    pub fn memory_bytes(&self) -> usize {
        self.text.len()
            + self
                .parts
                .iter()
                .map(|p| match p {
                    MessagePart::Text { text } => text.len(),
                    _ => 64,
                })
                .sum::<usize>()
            + self
                .attachments
                .iter()
                .map(|a| a.path.len() + a.name.len() + 256)
                .sum::<usize>()
            + self.forward.as_ref().map_or(0, String::len)
            + self.reaction_notice.as_ref().map_or(0, String::len)
            + 512
    }
    pub fn media_key(&self, element: &str) -> String {
        let key = format!(
            "{}:{}:{}:{}",
            self.account,
            self.peer.key(),
            self.id,
            element
        );
        if self.forward_path.is_empty() {
            key
        } else {
            format!("{key}:forward:{}", self.forward_path.join("/"))
        }
    }
    pub fn reaction_reference(&self) -> Option<ReactionNotice> {
        let xml = self.reaction_notice.as_ref()?;
        if xml.len() > 65536 {
            return None;
        }
        let doc = roxmltree::Document::parse(xml).ok()?;
        let seq = doc
            .descendants()
            .find(|n| n.has_tag_name("url"))?
            .attribute("msgseq")?;
        if seq.is_empty() || seq.len() > 20 || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let user = doc.descendants().find(|n| n.has_tag_name("qq"));
        let actor = user
            .and_then(|n| n.attribute("nm").or(n.text()).or(n.attribute("jp")))
            .unwrap_or("有人");
        Some(ReactionNotice {
            seq: seq.into(),
            actor: safe_text(actor),
        })
    }
    pub fn is_notice(&self) -> bool {
        self.system_notice || self.recalled || self.reaction_notice.is_some()
    }
    pub fn reference_seq(&self) -> Option<String> {
        self.reaction_reference()
            .map(|notice| notice.seq)
            .or_else(|| self.reply_seq.clone())
    }
    pub fn notice_text(&self) -> String {
        self.render_parts()
            .into_iter()
            .filter_map(|part| match part {
                MessagePart::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
    pub fn render_parts(&self) -> Vec<MessagePart> {
        if let Some(notice) = self.reaction_reference() {
            return vec![MessagePart::Text {
                text: format!("{} 添加了表情回应", notice.actor),
            }];
        }
        if !self.parts.is_empty() || self.forward.is_some() {
            return self.parts.clone();
        }
        // Older caches have no element positions. Preserve their text instead of guessing from face aliases.
        let mut parts = vec![MessagePart::Text {
            text: self.text.clone(),
        }];
        parts.extend(
            self.attachments
                .iter()
                .filter(|a| a.kind != "emoji")
                .map(|a| MessagePart::Attachment {
                    element_id: a.element_id.clone(),
                }),
        );
        parts
    }

    pub fn quote_key(&self) -> Option<String> {
        self.reply_to
            .as_ref()
            .filter(|id| !id.is_empty() && id.as_str() != "0")
            .cloned()
            .or_else(|| {
                self.reference_seq()
                    .as_ref()
                    .filter(|seq| !seq.is_empty() && seq.as_str() != "0")
                    .map(|seq| format!("seq:{seq}"))
            })
    }
    pub fn order_key(&self) -> (u64, u64, u64) {
        (
            self.time.parse().unwrap_or(0),
            self.seq.parse().unwrap_or(0),
            self.id.parse().unwrap_or(0),
        )
    }
    pub fn retract(&mut self) {
        self.touch();
        self.recalled = true;
        self.text = "消息已撤回".into();
        self.attachments.clear();
        self.parts.clear();
        self.forward = None;
        self.reactions.clear();
        self.reaction_notice = None;
        self.reply_to = None;
        self.reply_seq = None;
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub has_before: bool,
    pub has_after: bool,
    pub anchor_found: bool,
}
impl Default for HistoryPage {
    fn default() -> Self {
        Self {
            messages: vec![],
            has_before: false,
            has_after: false,
            anchor_found: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Draft {
    pub text: String,
    pub cursor: usize,
    pub reply_to: Option<String>,
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub mentions: Vec<Mention>,
}
impl Draft {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.attachments.is_empty()
            && self.reply_to.is_none()
            && self.mentions.is_empty()
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Mention {
    pub start: usize,
    pub end: usize,
    pub uid: String,
    pub uin: String,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    Submitting,
    Submitted,
    Confirmed,
    Failed,
    #[default]
    #[serde(other)]
    Unknown,
}
impl SendState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Submitting => "submitting",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}
impl From<&str> for SendState {
    fn from(s: &str) -> Self {
        match s {
            "submitting" => Self::Submitting,
            "submitted" => Self::Submitted,
            "confirmed" => Self::Confirmed,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}
impl PartialEq<&str> for SendState {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl std::fmt::Display for SendState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Receipt {
    pub request_id: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub peer: Peer,
    #[serde(default)]
    pub native_id: String,
    #[serde(default)]
    pub message_id: String,
    pub state: SendState,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub account: String,
    pub epoch: String,
    pub seq: u64,
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub unread_total: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostStatus {
    #[serde(default)]
    pub protocol_version: u32,
    pub phase: String,
    pub account: String,
    pub epoch: String,
    pub seq: u64,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
impl Default for HostStatus {
    fn default() -> Self {
        Self {
            protocol_version: 2,
            phase: String::new(),
            account: String::new(),
            epoch: String::new(),
            seq: 0,
            errors: vec![],
            capabilities: vec![],
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct BridgeEvent {
    pub epoch: String,
    pub seq: u64,
    pub account: String,
    pub kind: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    pub receipt: Option<Receipt>,
    pub message_peer: Option<Peer>,
    #[serde(default)]
    pub message_seq: String,
    pub notification: Option<crate::notifications::Batch>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Profile {
    pub uid: String,
    pub uin: String,
    pub nick: String,
    pub remark: String,
    pub signature: String,
}
pub fn safe_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reaction_notices_parse_exact_sequences_and_never_render_as_generic_system_messages() {
        let message=Message {reaction_notice:Some("<gtip><qq jp=\"123\" nm=\"Alice &amp; Bob\"/><url msgseq=\"18446744073709551615\"/><face id=\"76\"/></gtip>".into()),..Default::default()};
        let notice = message.reaction_reference().unwrap();
        assert_eq!(notice.seq, "18446744073709551615");
        assert_eq!(notice.actor, "Alice & Bob");
        assert!(
            matches!(message.render_parts().as_slice(),[MessagePart::Text {text}] if text.contains("表情回应") && !text.contains("系统消息"))
        );
        let invalid = Message {
            reaction_notice: Some("<gtip><url msgseq=\"bad\"/></gtip>".into()),
            ..Default::default()
        };
        assert!(invalid.reaction_reference().is_none());
    }
    #[test]
    fn recall_clears_reactions_and_structured_notification_content() {
        let mut message = Message {
            reactions: vec![Reaction::default()],
            reaction_notice: Some("notice".into()),
            ..Default::default()
        };
        message.retract();
        assert!(message.reactions.is_empty());
        assert!(message.reaction_notice.is_none());
        assert!(message.parts.is_empty());
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct SpeechPermission {
    pub state: String,
    pub scope: String,
    pub reason: String,
    pub until: Option<String>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Avatar {
    pub path: String,
    pub version: String,
}

impl SpeechPermission {
    pub fn description(&self) -> String {
        let mut text = if self.reason.is_empty() {
            match self.scope.as_str() {
                "all" => "全员禁言",
                "self" => "你已被禁言",
                _ => "群发言权限未知",
            }
            .into()
        } else {
            self.reason.clone()
        };
        if self.scope == "self"
            && let Some(until) = self
                .until
                .as_ref()
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        {
            text.push_str(&format!(
                "，至 {}",
                until.with_timezone(&chrono::Local).format("%m-%d %H:%M")
            ));
        }
        text
    }
}
