//! Media for rich messages: images and files embedded in replies
//! (`pageBlockPhoto` / `pageBlockDocument`) must be uploaded first with
//! `messages.uploadMedia`, then referenced by id in `inputRichMessage.photos`
//! / `.documents`. Uploads are cached per bot and media key, so edits,
//! restarts and repeated `/raw` never upload the same bytes twice.

use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use grammers_tl_types as tl;
use omni_core::BlobRef;
use rusqlite::{OptionalExtension, params};
use tracing::{info, warn};

use super::bot::BotConn;
use super::rich::{RichMedia, Uploaded};
use crate::app::App;
use crate::store::{Db, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Shown inline; falls back to a document when Telegram refuses it as a photo.
    Photo,
    Document,
}

impl Want {
    fn as_str(self) -> &'static str {
        match self {
            Want::Photo => "photo",
            Want::Document => "document",
        }
    }
}

/// One media item a message needs.
#[derive(Debug, Clone)]
pub struct Source {
    pub key: String,
    pub blob: BlobRef,
    pub filename: String,
    pub want: Want,
}

/// Make sure every source is uploaded; adds them to `media`.
pub async fn ensure(app: &App, bot: &BotConn, chat_id: i64, sources: &[Source], media: &mut RichMedia) -> Result<()> {
    for s in sources {
        if media.items.contains_key(&s.key) {
            continue;
        }
        if let Some(u) = cached(&app.db, &bot.name, &s.key, s.want).await? {
            media.items.insert(s.key.clone(), u);
            continue;
        }
        let bytes = app.blobs.get(&s.blob).await.with_context(|| format!("load {}", s.blob))?;
        let u = upload(bot, chat_id, bytes, &s.filename, &s.blob.mime, s.want).await?;
        store(&app.db, &bot.name, &s.key, s.want, &u).await?;
        media.items.insert(s.key.clone(), u);
    }
    Ok(())
}

/// Drop cached uploads (e.g. after a file-reference error) so they are uploaded again.
pub async fn forget(db: &Db, bot: &str, keys: &[String], media: &mut RichMedia) -> Result<()> {
    for k in keys {
        media.items.remove(k);
    }
    let (bot, keys) = (bot.to_string(), keys.to_vec());
    db.call(move |c| {
        for k in keys {
            c.execute("DELETE FROM tg_uploads WHERE bot = ?1 AND key = ?2", params![bot, k])?;
        }
        Ok(())
    })
    .await
}

async fn cached(db: &Db, bot: &str, key: &str, want: Want) -> Result<Option<Uploaded>> {
    let (bot, key) = (bot.to_string(), key.to_string());
    db.call(move |c| {
        let row: Option<(String, i64, i64, Vec<u8>)> = c
            .query_row(
                "SELECT kind, media_id, access_hash, file_reference FROM tg_uploads WHERE bot = ?1 AND key = ?2 AND want = ?3",
                params![bot, key, want.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row.map(|(kind, id, access_hash, file_reference)| match kind.as_str() {
            "photo" => Uploaded::Photo(tl::types::InputPhoto { id, access_hash, file_reference }),
            _ => Uploaded::Document(tl::types::InputDocument { id, access_hash, file_reference }),
        }))
    })
    .await
}

async fn store(db: &Db, bot: &str, key: &str, want: Want, u: &Uploaded) -> Result<()> {
    let (kind, id, hash, fr) = match u {
        Uploaded::Photo(p) => ("photo", p.id, p.access_hash, p.file_reference.clone()),
        Uploaded::Document(d) => ("document", d.id, d.access_hash, d.file_reference.clone()),
    };
    let (bot, key) = (bot.to_string(), key.to_string());
    db.call(move |c| {
        c.execute(
            "INSERT OR REPLACE INTO tg_uploads(bot, key, kind, want, media_id, access_hash, file_reference, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![bot, key, kind, want.as_str(), id, hash, fr, now_ms()],
        )?;
        Ok(())
    })
    .await
}

async fn upload_file(bot: &BotConn, bytes: &[u8], filename: &str) -> Result<tl::enums::InputFile> {
    let mut cursor = Cursor::new(bytes.to_vec());
    let up = bot
        .client
        .upload_stream(&mut cursor, bytes.len(), filename.to_string())
        .await
        .map_err(|e| anyhow!("upload {filename}: {e}"))?;
    Ok(up.raw)
}

async fn upload(
    bot: &BotConn,
    chat_id: i64,
    bytes: Vec<u8>,
    filename: &str,
    mime: &str,
    want: Want,
) -> Result<Uploaded> {
    let peer = bot.input_peer(chat_id).await?;
    if want == Want::Photo {
        let file = upload_file(bot, &bytes, filename).await?;
        let media = tl::types::InputMediaUploadedPhoto {
            spoiler: false,
            live_photo: false,
            file,
            stickers: None,
            ttl_seconds: None,
            video: None,
        }
        .into();
        let call = tl::functions::messages::UploadMedia { business_connection_id: None, peer: peer.clone(), media };
        match bot.invoke(&call).await {
            Ok(tl::enums::MessageMedia::Photo(tl::types::MessageMediaPhoto {
                photo: Some(tl::enums::Photo::Photo(p)),
                ..
            })) => {
                info!(bot = %bot.name, size = bytes.len(), "uploaded photo for a rich message");
                return Ok(Uploaded::Photo(tl::types::InputPhoto {
                    id: p.id,
                    access_hash: p.access_hash,
                    file_reference: p.file_reference,
                }));
            }
            Ok(other) => warn!(bot = %bot.name, ?other, "unexpected photo upload result; attaching as a file"),
            Err(e) => {
                warn!(bot = %bot.name, error = %e, size = bytes.len(), "Telegram refused the image as a photo; attaching as a file")
            }
        }
    }
    let file = upload_file(bot, &bytes, filename).await?;
    let media = tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        force_file: true,
        spoiler: false,
        file,
        thumb: None,
        mime_type: mime.to_string(),
        attributes: vec![tl::types::DocumentAttributeFilename { file_name: filename.to_string() }.into()],
        stickers: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    }
    .into();
    let call = tl::functions::messages::UploadMedia { business_connection_id: None, peer, media };
    match bot.invoke(&call).await.map_err(|e| anyhow!("upload {filename} as a file: {e}"))? {
        tl::enums::MessageMedia::Document(tl::types::MessageMediaDocument {
            document: Some(tl::enums::Document::Document(d)),
            ..
        }) => {
            info!(bot = %bot.name, size = bytes.len(), filename, "uploaded file for a rich message");
            Ok(Uploaded::Document(tl::types::InputDocument {
                id: d.id,
                access_hash: d.access_hash,
                file_reference: d.file_reference,
            }))
        }
        other => bail!("unexpected document upload result: {other:?}"),
    }
}
