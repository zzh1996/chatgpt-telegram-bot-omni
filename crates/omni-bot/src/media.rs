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
    /// Sticker / animated emoji etc. (not sent to models).
    #[serde(default)]
    pub is_sticker: bool,
}

impl MediaRef {
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
            MediaKind::Document => tl::types::InputDocumentFileLocation {
                id: m.id,
                access_hash: m.access_hash,
                file_reference,
                thumb_size: String::new(),
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
    }
}

fn document_ref(doc: &tl::types::Document, msg: &tl::types::Message) -> MediaRef {
    let mut filename = None;
    let mut is_sticker = false;
    for a in &doc.attributes {
        match a {
            tl::enums::DocumentAttribute::Filename(f) => filename = Some(f.file_name.clone()),
            tl::enums::DocumentAttribute::Sticker(_) | tl::enums::DocumentAttribute::CustomEmoji(_) => {
                is_sticker = true
            }
            _ => {}
        }
    }
    MediaRef {
        kind: MediaKind::Document,
        id: doc.id,
        access_hash: doc.access_hash,
        file_reference: base64::engine::general_purpose::STANDARD.encode(&doc.file_reference),
        dc_id: doc.dc_id,
        size: Some(doc.size as u64),
        thumb_size: String::new(),
        mime: normalize_mime(&doc.mime_type, filename.as_deref()),
        filename,
        msg_id: msg.id,
        grouped_id: msg.grouped_id,
        is_sticker,
    }
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
        bail!("{} is too large ({} MB > {} MB limit)", m.describe(), s / 1_000_000, max_bytes / 1_000_000);
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
