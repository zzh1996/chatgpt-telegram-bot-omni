//! Telegram media: descriptors, deduplicated downloads, album collection.
//!
//! Downloads are keyed by Telegram's photo/document `id`, which is stable
//! across forwards and resends, so the same file is downloaded once.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use grammers_client::media::Downloadable;
use grammers_tl_types as tl;
use omni_core::BlobRef;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::store::blobs::BlobStore;
use crate::store::{Db, now_ms};
use crate::tg::bot::BotConn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Document,
}

/// What a Telegram media item is, as the user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TgKind {
    #[default]
    File,
    Photo,
    Voice,
    Audio,
    Video,
    /// A round video message.
    VideoNote,
    /// A GIF (an MP4 without sound).
    Animation,
    /// A static sticker (WebP).
    Sticker,
    /// A video sticker (WebM).
    VideoSticker,
    /// An animated (Lottie) sticker: no model reads the animation, so its still
    /// preview is downloaded instead (`thumb_size`), and the reply says so.
    AnimatedSticker,
}

/// File name of the still preview sent for an animated (Lottie) sticker.
pub const ANIMATED_STICKER_PREVIEW: &str = "animated-sticker-preview.webp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub id: i64,
    pub access_hash: i64,
    /// base64 of the file reference.
    pub file_reference: String,
    pub dc_id: i32,
    #[serde(default)]
    pub size: Option<u64>,
    /// Photo size type to download (largest), empty for documents.
    #[serde(default)]
    pub thumb_size: String,
    pub mime: String,
    #[serde(default)]
    pub filename: Option<String>,
    /// Message where this media appeared (to refresh expired file references).
    pub msg_id: i32,
    #[serde(default)]
    pub grouped_id: Option<i64>,
    /// Sticker / animated emoji etc.
    #[serde(default)]
    pub is_sticker: bool,
    #[serde(default)]
    pub tg_kind: TgKind,
}

impl MediaRef {
    /// The file's name, or what it is (`photo`, its type) when it has none.
    pub fn name(&self) -> String {
        match (&self.filename, self.kind) {
            (Some(f), _) => f.clone(),
            (None, MediaKind::Photo) => "the photo".into(),
            (None, MediaKind::Document) => format!("the {} file", self.mime),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}{}",
            match self.kind {
                MediaKind::Photo => "photo",
                MediaKind::Document => "file",
            },
            self.filename.clone().unwrap_or_else(|| self.mime.clone()),
            self.size.map(|s| format!(" ({} KB)", s / 1024)).unwrap_or_default()
        )
    }
}

struct Loc<'a>(&'a MediaRef);

impl Downloadable for Loc<'_> {
    fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
        let m = self.0;
        let file_reference = base64::engine::general_purpose::STANDARD.decode(&m.file_reference).ok()?;
        Some(match m.kind {
            MediaKind::Photo => tl::types::InputPhotoFileLocation {
                id: m.id,
                access_hash: m.access_hash,
                file_reference,
                thumb_size: m.thumb_size.clone(),
            }
            .into(),
            // A document's preview when `thumb_size` names one (animated stickers).
            MediaKind::Document => tl::types::InputDocumentFileLocation {
                id: m.id,
                access_hash: m.access_hash,
                file_reference,
                thumb_size: m.thumb_size.clone(),
            }
            .into(),
        })
    }
    fn size(&self) -> Option<usize> {
        self.0.size.map(|s| s as usize)
    }
    fn dc_id(&self) -> Option<i32> {
        Some(self.0.dc_id)
    }
}

fn photo_ref(photo: &tl::types::Photo, msg: &tl::types::Message) -> MediaRef {
    let (thumb, size) = largest_photo_size(&photo.sizes);
    MediaRef {
        kind: MediaKind::Photo,
        id: photo.id,
        access_hash: photo.access_hash,
        file_reference: base64::engine::general_purpose::STANDARD.encode(&photo.file_reference),
        dc_id: photo.dc_id,
        size,
        thumb_size: thumb,
        mime: "image/jpeg".into(),
        filename: None,
        msg_id: msg.id,
        grouped_id: msg.grouped_id,
        is_sticker: false,
        tg_kind: TgKind::Photo,
    }
}

fn document_ref(doc: &tl::types::Document, msg: &tl::types::Message) -> MediaRef {
    use tl::enums::DocumentAttribute as A;
    let mut filename = None;
    let mut is_sticker = false;
    let (mut voice, mut audio, mut video, mut round, mut animated) = (false, false, false, false, false);
    for a in &doc.attributes {
        match a {
            A::Filename(f) => filename = Some(f.file_name.clone()),
            A::Sticker(_) | A::CustomEmoji(_) => is_sticker = true,
            A::Audio(x) => (audio, voice) = (true, x.voice),
            A::Video(x) => (video, round) = (true, x.round_message),
            A::Animated => animated = true,
            _ => {}
        }
    }
    let mime = normalize_mime(&doc.mime_type, filename.as_deref());
    let tg_kind = match () {
        _ if is_sticker && mime == "application/x-tgsticker" => TgKind::AnimatedSticker,
        _ if is_sticker && mime.starts_with("video/") => TgKind::VideoSticker,
        _ if is_sticker => TgKind::Sticker,
        _ if voice => TgKind::Voice,
        _ if audio => TgKind::Audio,
        _ if round => TgKind::VideoNote,
        _ if animated => TgKind::Animation,
        _ if video => TgKind::Video,
        _ => TgKind::File,
    };
    // Telegram has no file name for these; models and /raw get a sensible one.
    let ext = |m: &str, default: &str| {
        mime_guess::get_mime_extensions_str(m).and_then(|e| e.first().copied()).unwrap_or(default).to_string()
    };
    let filename = filename.or_else(|| match tg_kind {
        TgKind::Voice => Some(format!("voice.{}", ext(&mime, "ogg"))),
        TgKind::VideoNote => Some("video-message.mp4".into()),
        TgKind::Animation => Some("animation.mp4".into()),
        TgKind::Sticker => Some("sticker.webp".into()),
        TgKind::VideoSticker => Some("sticker.webm".into()),
        _ => None,
    });
    let mut r = MediaRef {
        kind: MediaKind::Document,
        id: doc.id,
        access_hash: doc.access_hash,
        file_reference: base64::engine::general_purpose::STANDARD.encode(&doc.file_reference),
        dc_id: doc.dc_id,
        size: Some(doc.size as u64),
        thumb_size: String::new(),
        mime,
        filename,
        msg_id: msg.id,
        grouped_id: msg.grouped_id,
        is_sticker,
        tg_kind,
    };
    if tg_kind == TgKind::AnimatedSticker {
        // The largest still preview Telegram keeps for it (a WebP image).
        let (thumb, size) = largest_photo_size(doc.thumbs.as_deref().unwrap_or(&[]));
        r.thumb_size = thumb;
        r.size = size;
        r.mime = "image/webp".into();
        r.filename = Some(ANIMATED_STICKER_PREVIEW.into());
    }
    r
}

/// Extract media descriptors from a message: its photo or file, or the photos
/// and files inside a rich message (in block order).
pub fn media_of(msg: &tl::types::Message) -> Vec<MediaRef> {
    let mut out = match &msg.media {
        Some(tl::enums::MessageMedia::Photo(p)) => match &p.photo {
            Some(tl::enums::Photo::Photo(photo)) => vec![photo_ref(photo, msg)],
            _ => Vec::new(),
        },
        Some(tl::enums::MessageMedia::Document(d)) => match &d.document {
            Some(tl::enums::Document::Document(doc)) => vec![document_ref(doc, msg)],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if let Some(tl::enums::RichMessage::Message(rm)) = &msg.rich_message {
        let c = crate::tg::richin::convert(&rm.blocks);
        let photos: Vec<&tl::types::Photo> = rm
            .photos
            .iter()
            .filter_map(|p| match p {
                tl::enums::Photo::Photo(p) => Some(p),
                _ => None,
            })
            .collect();
        let docs: Vec<&tl::types::Document> = rm
            .documents
            .iter()
            .filter_map(|d| match d {
                tl::enums::Document::Document(d) => Some(d),
                _ => None,
            })
            .collect();
        for id in &c.photo_ids {
            if let Some(p) = photos.iter().find(|p| p.id == *id) {
                out.push(photo_ref(p, msg));
            }
        }
        for id in &c.document_ids {
            if let Some(d) = docs.iter().find(|d| d.id == *id) {
                out.push(document_ref(d, msg));
            }
        }
        out.dedup_by_key(|m| m.id);
    }
    out
}

/// A position shared in Telegram (location, venue or live location; map blocks
/// of rich messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocationRef {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy_m: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

impl LocationRef {
    pub fn part(&self) -> omni_core::content::Part {
        omni_core::content::Part::Location {
            latitude: self.latitude,
            longitude: self.longitude,
            accuracy_m: self.accuracy_m,
            title: self.title.clone(),
            address: self.address.clone(),
        }
    }
}

/// The positions in a message: a location / venue / live location (its
/// current position), or the map blocks of a rich message.
pub fn locations_of(msg: &tl::types::Message) -> Vec<LocationRef> {
    let point = |g: &tl::enums::GeoPoint| match g {
        tl::enums::GeoPoint::Point(p) => Some(LocationRef {
            latitude: p.lat,
            longitude: p.long,
            accuracy_m: p.accuracy_radius.map(|r| r.max(0) as u32),
            title: None,
            address: None,
        }),
        tl::enums::GeoPoint::Empty => None,
    };
    let mut out: Vec<LocationRef> = match &msg.media {
        Some(tl::enums::MessageMedia::Geo(m)) => point(&m.geo).into_iter().collect(),
        Some(tl::enums::MessageMedia::GeoLive(m)) => point(&m.geo).into_iter().collect(),
        Some(tl::enums::MessageMedia::Venue(v)) => point(&v.geo)
            .map(|mut l| {
                l.title = Some(v.title.clone()).filter(|s| !s.is_empty());
                l.address = Some(v.address.clone()).filter(|s| !s.is_empty());
                l
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    if let Some(tl::enums::RichMessage::Message(rm)) = &msg.rich_message {
        for (lat, lon, acc) in crate::tg::richin::convert(&rm.blocks).maps {
            out.push(LocationRef { latitude: lat, longitude: lon, accuracy_m: acc, title: None, address: None });
        }
    }
    out
}

/// Telegram messages that are neither text nor files, as text for the model:
/// a poll (question, options, votes when known), a contact, a die and its
/// value, a to-do list.
pub fn cards_of(msg: &tl::types::Message) -> Vec<String> {
    use tl::enums::MessageMedia as M;
    let text = |t: &tl::enums::TextWithEntities| {
        let tl::enums::TextWithEntities::Entities(t) = t;
        t.text.clone()
    };
    match &msg.media {
        Some(M::Poll(p)) => {
            let tl::enums::Poll::Poll(poll) = &p.poll;
            let tl::enums::PollResults::Results(res) = &p.results;
            let votes = |option: &[u8]| {
                res.results.iter().flatten().find_map(|v| {
                    let tl::enums::PollAnswerVoters::Voters(v) = v;
                    (v.option == option).then_some((v.voters, v.correct))
                })
            };
            let mut kind = Vec::new();
            if poll.quiz {
                kind.push("quiz");
            }
            if poll.multiple_choice {
                kind.push("multiple answers");
            }
            if poll.closed {
                kind.push("closed");
            }
            let mut out = format!(
                "📊 Poll{}: {}",
                if kind.is_empty() { String::new() } else { format!(" ({})", kind.join(", ")) },
                text(&poll.question)
            );
            for a in &poll.answers {
                let tl::enums::PollAnswer::Answer(a) = a else { continue };
                out.push_str(&format!("\n- {}", text(&a.text)));
                if let Some((n, correct)) = votes(&a.option) {
                    if let Some(n) = n {
                        out.push_str(&format!(" — {n} votes"));
                    }
                    if correct {
                        out.push_str(" (correct)");
                    }
                }
            }
            if let Some(n) = res.total_voters {
                out.push_str(&format!("\n{n} voters"));
            }
            vec![out]
        }
        Some(M::Contact(c)) => {
            let name = format!("{} {}", c.first_name, c.last_name).trim().to_string();
            let mut out = format!("👤 Contact: {name}, {}", c.phone_number);
            if !c.vcard.trim().is_empty() {
                out.push_str(&format!("\n{}", c.vcard.trim()));
            }
            vec![out]
        }
        Some(M::Dice(d)) => vec![format!("{} (Telegram dice) = {}", d.emoticon, d.value)],
        Some(M::ToDo(t)) => {
            let tl::enums::TodoList::List(list) = &t.todo;
            let done: Vec<i32> = t
                .completions
                .iter()
                .flatten()
                .map(|c| {
                    let tl::enums::TodoCompletion::Completion(c) = c;
                    c.id
                })
                .collect();
            let mut out = format!("☑️ {}", text(&list.title));
            for it in &list.list {
                let tl::enums::TodoItem::Item(it) = it;
                out.push_str(&format!("\n- [{}] {}", if done.contains(&it.id) { "x" } else { " " }, text(&it.title)));
            }
            vec![out]
        }
        _ => Vec::new(),
    }
}

/// The text of a message for the bot: a rich message as Markdown, otherwise
/// the message text (or caption) as it is.
pub fn text_of(msg: &tl::types::Message) -> String {
    match &msg.rich_message {
        Some(tl::enums::RichMessage::Message(rm)) => crate::tg::richin::convert(&rm.blocks).markdown,
        None => msg.message.clone(),
    }
}

/// Telegram sometimes reports `application/octet-stream` for known types.
fn normalize_mime(mime: &str, filename: Option<&str>) -> String {
    if mime != "application/octet-stream" && !mime.is_empty() {
        return mime.to_string();
    }
    filename
        .and_then(|f| mime_guess::from_path(f).first())
        .map(|m| m.essence_str().to_string())
        .unwrap_or_else(|| mime.to_string())
}

fn largest_photo_size(sizes: &[tl::enums::PhotoSize]) -> (String, Option<u64>) {
    let mut best: Option<(String, i64, Option<u64>)> = None;
    for s in sizes {
        let (ty, area, size) = match s {
            tl::enums::PhotoSize::Size(p) => (p.r#type.clone(), p.w as i64 * p.h as i64, Some(p.size as u64)),
            tl::enums::PhotoSize::Progressive(p) => {
                (p.r#type.clone(), p.w as i64 * p.h as i64, p.sizes.iter().max().map(|&x| x as u64))
            }
            tl::enums::PhotoSize::PhotoCachedSize(p) => {
                (p.r#type.clone(), p.w as i64 * p.h as i64, Some(p.bytes.len() as u64))
            }
            _ => continue,
        };
        if best.as_ref().is_none_or(|b| area > b.1) {
            best = Some((ty, area, size));
        }
    }
    best.map(|(t, _, s)| (t, s)).unwrap_or_else(|| ("w".into(), None))
}

/// Download (or reuse) the bytes of a media item as a blob.
pub async fn fetch(
    db: &Db,
    blobs: &BlobStore,
    bot: &BotConn,
    chat_id: i64,
    m: &MediaRef,
    max_bytes: u64,
) -> Result<BlobRef> {
    let kind = match m.kind {
        MediaKind::Photo => "photo",
        MediaKind::Document => "document",
    };
    let file_id = m.id;
    let cached: Option<(String, i64, Option<String>)> = db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT sha256, size, mime FROM tg_files WHERE kind = ?1 AND file_id = ?2",
                params![kind, file_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
        })
        .await?;
    if let Some((sha, size, _)) = cached
        && blobs.exists(&sha)
    {
        debug!(file_id, "media cache hit");
        return Ok(BlobRef { sha256: sha, size: size as u64, mime: m.mime.clone() });
    }
    if let Some(s) = m.size
        && s > max_bytes
    {
        bail!("too large: {:.1} MB, the limit is {} MB", s as f64 / 1_000_000.0, max_bytes / 1_000_000);
    }
    let bytes = match download(bot, m, max_bytes).await {
        Ok(b) => b,
        Err(e) if format!("{e}").contains("FILE_REFERENCE") => {
            info!(file_id, "file reference expired; refetching message {}", m.msg_id);
            let msgs = bot.get_messages(chat_id, &[m.msg_id]).await?;
            let fresh = msgs
                .iter()
                .filter_map(|x| match x {
                    tl::enums::Message::Message(x) => Some(media_of(x)),
                    _ => None,
                })
                .flatten()
                .find(|x| x.id == m.id)
                .ok_or_else(|| anyhow!("media no longer available"))?;
            download(bot, &fresh, max_bytes).await?
        }
        Err(e) => return Err(e),
    };
    let r = blobs.put(bytes, m.mime.clone()).await?;
    let (sha, size, mime) = (r.sha256.clone(), r.size as i64, r.mime.clone());
    db.call(move |c| {
        c.execute(
            "INSERT OR REPLACE INTO tg_files(kind, file_id, sha256, size, mime) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, file_id, sha, size, mime],
        )?;
        let _ = now_ms();
        Ok(())
    })
    .await?;
    info!(file_id, sha = %r.short(), size = r.size, "media downloaded");
    Ok(r)
}

async fn download(bot: &BotConn, m: &MediaRef, max_bytes: u64) -> Result<Vec<u8>> {
    let mut it = bot.client.iter_download(&Loc(m));
    let mut out = Vec::new();
    while let Some(chunk) = it.next().await.map_err(|e| anyhow!("{e}"))? {
        out.extend_from_slice(&chunk);
        if out.len() as u64 > max_bytes {
            bail!("{} exceeds the {} MB limit", m.describe(), max_bytes / 1_000_000);
        }
    }
    Ok(out)
}

/// One message of an album.
pub struct AlbumItem {
    pub msg_id: i32,
    pub media: Vec<MediaRef>,
    /// Its caption (Markdown for a rich message).
    pub text: String,
}

/// All items of an album around `msg_id` (an album has at most 10 items, with
/// adjacent ids), in order.
pub async fn album(bot: &BotConn, chat_id: i64, msg_id: i32, grouped_id: i64) -> Result<Vec<AlbumItem>> {
    let ids: Vec<i32> = ((msg_id - 10).max(1)..=msg_id + 10).collect();
    let msgs = bot.get_messages(chat_id, &ids).await.context("fetch album parts")?;
    let mut items: Vec<AlbumItem> = msgs
        .iter()
        .filter_map(|m| match m {
            tl::enums::Message::Message(m) if m.grouped_id == Some(grouped_id) => {
                Some(AlbumItem { msg_id: m.id, media: media_of(m), text: text_of(m) })
            }
            _ => None,
        })
        .collect();
    items.sort_by_key(|i| i.msg_id);
    items.dedup_by_key(|i| i.msg_id);
    Ok(items)
}

/// Duration (seconds) and video size of an MP4/MOV file, from its `mvhd` and
/// the first `tkhd` with a size (no decoding).
pub fn mp4_info(b: &[u8]) -> Option<(f64, u32, u32)> {
    fn be32(b: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
    }
    fn be64(b: &[u8], at: usize) -> Option<u64> {
        Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
    }
    // Boxes inside `b[from..to]`: (type, payload start, end).
    fn boxes(b: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        let mut at = from;
        while at + 8 <= to {
            let Some(size) = be32(b, at) else { break };
            let kind: [u8; 4] = b[at + 4..at + 8].try_into().unwrap();
            let (start, end) = match size {
                1 => match be64(b, at + 8) {
                    Some(s) if s >= 16 => (at + 16, at + s as usize),
                    _ => break,
                },
                0 => (at + 8, to),
                s if s >= 8 => (at + 8, at + s as usize),
                _ => break,
            };
            if end > to {
                break;
            }
            out.push((kind, start, end));
            at = end;
        }
        out
    }
    let moov = boxes(b, 0, b.len()).into_iter().find(|(k, ..)| k == b"moov")?;
    let inner = boxes(b, moov.1, moov.2);
    let mvhd = inner.iter().find(|(k, ..)| k == b"mvhd")?;
    let duration = match b.get(mvhd.1)? {
        1 => be64(b, mvhd.1 + 24)? as f64 / be32(b, mvhd.1 + 20)?.max(1) as f64,
        _ => be32(b, mvhd.1 + 16)? as f64 / be32(b, mvhd.1 + 12)?.max(1) as f64,
    };
    let (mut w, mut h) = (0, 0);
    for (_, s, e) in inner.iter().filter(|(k, ..)| k == b"trak") {
        if let Some(t) = boxes(b, *s, *e).into_iter().find(|(k, ..)| k == b"tkhd") {
            // Width and height are the last 8 bytes, as 16.16 fixed point.
            let (tw, th) = (be32(b, t.2 - 8)? >> 16, be32(b, t.2 - 4)? >> 16);
            if tw > 0 && th > 0 {
                (w, h) = (tw, th);
                break;
            }
        }
    }
    Some((duration, w, h))
}

/// Duration (seconds) of a WAV file, from its header.
pub fn wav_seconds(b: &[u8]) -> Option<f64> {
    if b.get(0..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut byte_rate = None;
    while at + 8 <= b.len() {
        let size = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        match &b[at..at + 4] {
            b"fmt " => byte_rate = Some(u32::from_le_bytes(b.get(at + 16..at + 20)?.try_into().ok()?)),
            b"data" => return Some(size as f64 / byte_rate?.max(1) as f64),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    None
}

#[cfg(test)]
mod probe_tests {
    #[test]
    fn mp4_and_wav_headers() {
        // A 2-second 64×48 H.264 clip made with ffmpeg.
        let mp4 = include_bytes!("../tests/fixtures/tiny-64x48-2s.mp4");
        let (d, w, h) = super::mp4_info(mp4).unwrap();
        assert!((d - 2.0).abs() < 0.1, "{d}");
        assert_eq!((w, h), (64, 48));
        assert!(super::mp4_info(b"not a video").is_none());
        // 1.5 s of 16-bit mono at 8 kHz: 24,000 bytes of data at 16,000 bytes/s.
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend(16u32.to_le_bytes());
        wav.extend([1, 0, 1, 0]);
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend([2, 0, 16, 0]);
        wav.extend(b"data");
        wav.extend(24000u32.to_le_bytes());
        wav.extend(vec![0u8; 24000]);
        assert_eq!(super::wav_seconds(&wav), Some(1.5));
        assert!(super::wav_seconds(mp4).is_none());
    }
}
