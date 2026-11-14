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
    /// A document with video attributes (playable in `pageBlockVideo`).
    Video,
    /// A document with audio attributes (playable in `pageBlockAudio`).
    Audio,
}

impl Want {
    fn as_str(self) -> &'static str {
        match self {
            Want::Photo => "photo",
            Want::Document => "document",
            Want::Video => "video",
            Want::Audio => "audio",
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
    let mut attributes: Vec<tl::enums::DocumentAttribute> =
        vec![tl::types::DocumentAttributeFilename { file_name: safe_filename(filename) }.into()];
    match want {
        Want::Video => {
            let (duration, w, h) = crate::media::mp4_info(&bytes).unwrap_or((0.0, 0, 0));
            attributes.push(
                tl::types::DocumentAttributeVideo {
                    round_message: false,
                    supports_streaming: true,
                    nosound: false,
                    duration,
                    w: w as i32,
                    h: h as i32,
                    preload_prefix_size: None,
                    video_start_ts: None,
                    video_codec: None,
                }
                .into(),
            );
        }
        Want::Audio => attributes.push(
            tl::types::DocumentAttributeAudio {
                voice: false,
                duration: crate::media::wav_seconds(&bytes).unwrap_or(0.0).round() as i32,
                title: None,
                performer: None,
                waveform: None,
            }
            .into(),
        ),
        Want::Photo | Want::Document => {}
    }
    let file = upload_file(bot, &bytes, filename).await?;
    let media = tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        force_file: matches!(want, Want::Photo | Want::Document),
        spoiler: false,
        file,
        thumb: None,
        mime_type: mime.to_string(),
        attributes,
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

/// A file name as Telegram shows it. Names can come from models and providers
/// (a code tool's output files): control characters, bidi overrides and
/// zero-width characters are removed (`report\u{202E}fdp.exe` would read
/// `reportexe.pdf`), path separators become `_`, and long names are shortened
/// with their extension kept.
pub fn safe_filename(name: &str) -> String {
    const MAX_CHARS: usize = 100;
    let invisible = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}')
    };
    let clean: String =
        name.chars().filter(|&c| !invisible(c)).map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return "file".into();
    }
    if clean.chars().count() <= MAX_CHARS {
        return clean.to_string();
    }
    // Keep a short extension (`.pdf`) after the shortened stem.
    let ext = clean.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.is_empty() && e.chars().count() <= 10);
    let keep = MAX_CHARS - ext.map_or(0, |e| e.chars().count() + 1);
    let stem: String = clean.chars().take(keep).collect();
    match ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::safe_filename;

    #[test]
    fn file_names_show_what_they_are() {
        assert_eq!(safe_filename("plot.png"), "plot.png");
        assert_eq!(safe_filename("报告 2026.pdf"), "报告 2026.pdf");
        // A right-to-left override would make `report<RLO>fdp.exe` read `reportexe.pdf`.
        assert_eq!(safe_filename("report\u{202E}fdp.exe"), "reportfdp.exe");
        assert_eq!(safe_filename("a\u{200B}b\u{2066}c\nd\te.txt"), "abcde.txt");
        assert_eq!(safe_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_filename(" \u{202E} "), "file");
        let long = format!("{}.pdf", "x".repeat(300));
        let short = safe_filename(&long);
        assert_eq!(short.chars().count(), 100);
        assert!(short.ends_with(".pdf"));
    }
}
