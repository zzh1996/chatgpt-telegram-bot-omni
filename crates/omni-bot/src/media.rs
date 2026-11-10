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

/// Extract media descriptors from a message.
pub fn media_of(msg: &tl::types::Message) -> Vec<MediaRef> {
    let Some(media) = &msg.media else { return Vec::new() };
    match media {
        tl::enums::MessageMedia::Photo(p) => match &p.photo {
            Some(tl::enums::Photo::Photo(photo)) => {
                let (thumb, size) = largest_photo_size(&photo.sizes);
                vec![MediaRef {
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
                }]
            }
            _ => Vec::new(),
        },
        tl::enums::MessageMedia::Document(d) => match &d.document {
            Some(tl::enums::Document::Document(doc)) => {
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
                vec![MediaRef {
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
                }]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
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

/// Collect all parts of an album around `msg_id` (album parts have adjacent ids).
pub async fn album(bot: &BotConn, chat_id: i64, msg_id: i32, grouped_id: i64) -> Result<Vec<MediaRef>> {
    let ids: Vec<i32> = ((msg_id - 10).max(1)..=msg_id + 10).collect();
    let msgs = bot.get_messages(chat_id, &ids).await.context("fetch album parts")?;
    let mut parts: Vec<MediaRef> = msgs
        .iter()
        .filter_map(|m| match m {
            tl::enums::Message::Message(m) if m.grouped_id == Some(grouped_id) => Some(media_of(m)),
            _ => None,
        })
        .flatten()
        .collect();
    parts.sort_by_key(|p| p.msg_id);
    parts.dedup_by_key(|p| p.id);
    Ok(parts)
}
