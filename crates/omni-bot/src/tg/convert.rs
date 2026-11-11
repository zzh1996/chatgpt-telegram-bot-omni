//! Conversions between our types and Telegram TL types.

use grammers_tl_types as tl;
use omni_markdown::{EntityKind, Formatted, TgEntity};

pub fn entity_to_tl(e: &TgEntity) -> tl::enums::MessageEntity {
    let (offset, length) = (e.offset, e.length);
    match &e.kind {
        EntityKind::Bold => tl::types::MessageEntityBold { offset, length }.into(),
        EntityKind::Italic => tl::types::MessageEntityItalic { offset, length }.into(),
        EntityKind::Underline => tl::types::MessageEntityUnderline { offset, length }.into(),
        EntityKind::Strikethrough => tl::types::MessageEntityStrike { offset, length }.into(),
        EntityKind::Spoiler => tl::types::MessageEntitySpoiler { offset, length }.into(),
        EntityKind::Code => tl::types::MessageEntityCode { offset, length }.into(),
        EntityKind::Pre { language } => {
            tl::types::MessageEntityPre { offset, length, language: language.clone() }.into()
        }
        EntityKind::TextUrl { url } => tl::types::MessageEntityTextUrl { offset, length, url: url.clone() }.into(),
        EntityKind::Blockquote { collapsed } => {
            tl::types::MessageEntityBlockquote { collapsed: *collapsed, offset, length }.into()
        }
    }
}

/// Text + TL entities for a formatted chunk.
pub fn formatted_to_tl(f: &Formatted) -> (String, Vec<tl::enums::MessageEntity>) {
    let (text, ents) = f.to_telegram();
    (text, ents.iter().map(entity_to_tl).collect())
}

/// The kind of chat, derived from a Bot-API-style marked chat id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Private,
    Group,
    Channel,
}

pub fn chat_kind(marked_id: i64) -> ChatKind {
    if marked_id > 0 {
        ChatKind::Private
    } else if marked_id >= -999_999_999_999 {
        ChatKind::Group
    } else {
        ChatKind::Channel
    }
}

/// Marked (Bot API style) id of a TL peer.
pub fn marked_peer_id(p: &tl::enums::Peer) -> i64 {
    match p {
        tl::enums::Peer::User(u) => u.user_id,
        tl::enums::Peer::Chat(c) => -c.chat_id,
        tl::enums::Peer::Channel(c) => -(1_000_000_000_000 + c.channel_id),
    }
}

/// Extract the message id assigned to `random_id` from the response of a send call.
pub fn sent_message_id(updates: &tl::enums::Updates, random_id: i64) -> Option<i32> {
    let find = |list: &[tl::enums::Update]| {
        list.iter().find_map(|u| match u {
            tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(m.id),
            _ => None,
        })
    };
    match updates {
        tl::enums::Updates::UpdateShortSentMessage(s) => Some(s.id),
        tl::enums::Updates::Updates(u) => find(&u.updates).or_else(|| single_new_message_id(&u.updates)),
        tl::enums::Updates::Combined(u) => find(&u.updates).or_else(|| single_new_message_id(&u.updates)),
        tl::enums::Updates::UpdateShort(u) => match &u.update {
            tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(m.id),
            _ => None,
        },
        _ => None,
    }
}

fn single_new_message_id(list: &[tl::enums::Update]) -> Option<i32> {
    let ids: Vec<i32> = list
        .iter()
        .filter_map(|u| match u {
            tl::enums::Update::NewMessage(m) => message_id(&m.message),
            tl::enums::Update::NewChannelMessage(m) => message_id(&m.message),
            _ => None,
        })
        .collect();
    if ids.len() == 1 { Some(ids[0]) } else { None }
}

pub fn message_id(m: &tl::enums::Message) -> Option<i32> {
    match m {
        tl::enums::Message::Message(m) => Some(m.id),
        tl::enums::Message::Service(m) => Some(m.id),
        tl::enums::Message::Empty(m) => Some(m.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_kinds() {
        assert_eq!(chat_kind(12345678), ChatKind::Private);
        assert_eq!(chat_kind(-123456), ChatKind::Group);
        assert_eq!(chat_kind(-1001234567890), ChatKind::Channel);
        assert_eq!(marked_peer_id(&tl::types::PeerChannel { channel_id: 1234567890 }.into()), -1001234567890);
        assert_eq!(marked_peer_id(&tl::types::PeerChat { chat_id: 123456 }.into()), -123456);
    }

    #[test]
    fn short_sent_message_id() {
        let u: tl::enums::Updates = tl::types::UpdateShortSentMessage {
            out: true,
            id: 77,
            pts: 1,
            pts_count: 1,
            date: 0,
            media: None,
            entities: None,
            ttl_period: None,
        }
        .into();
        assert_eq!(sent_message_id(&u, 5), Some(77));
    }
}
