//! JSON helpers.

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::blob::BlobRef;

/// RFC 7396 JSON Merge Patch: objects merge recursively, `null` deletes a key,
/// anything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let t = target.as_object_mut().unwrap();
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *target = other.clone(),
    }
}

/// Strings at least this long that look like base64 are treated as blobs in logs.
pub const BLOB_THRESHOLD: usize = 2048;

/// Replace large binary payloads (data URLs, long base64 strings) by blob
/// references. `store` persists the decoded bytes and returns their reference,
/// so no information is lost: the log keeps the hash, the blob store keeps the
/// bytes. Returns a redacted copy.
pub fn redact_blobs(v: &Value, store: &mut dyn FnMut(&[u8], &str) -> BlobRef) -> Value {
    match v {
        Value::String(s) => match blob_of_string(s) {
            Some((bytes, mime, prefix)) => {
                let r = store(&bytes, &mime);
                Value::String(format!("{prefix}{r}"))
            }
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| redact_blobs(x, store)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), redact_blobs(x, store))).collect()),
        _ => v.clone(),
    }
}

/// If `s` is a data URL or a long base64 string, return (bytes, mime, display prefix).
pub fn blob_of_string(s: &str) -> Option<(Vec<u8>, String, String)> {
    if s.len() < BLOB_THRESHOLD {
        return None;
    }
    if let Some(rest) = s.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let mime = meta.split(';').next().unwrap_or("application/octet-stream").to_string();
        if meta.ends_with(";base64") {
            let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
            return Some((bytes, mime.clone(), format!("data:{mime};base64,")));
        }
        return None;
    }
    let looks_b64 = s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'));
    if !looks_b64 {
        return None;
    }
    let engine = if s.contains(['-', '_']) {
        &base64::engine::general_purpose::URL_SAFE
    } else {
        &base64::engine::general_purpose::STANDARD
    };
    let bytes = engine.decode(s).or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s)).ok()?;
    Some((bytes, "application/octet-stream".into(), "base64:".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_patch_semantics() {
        let mut t = json!({"a": 1, "b": {"c": 2, "d": 3}, "e": [1]});
        merge_patch(&mut t, &json!({"a": null, "b": {"c": 9}, "e": [2], "f": "x"}));
        assert_eq!(t, json!({"b": {"c": 9, "d": 3}, "e": [2], "f": "x"}));
    }

    #[test]
    fn redacts_data_urls() {
        let payload = base64::engine::general_purpose::STANDARD.encode(vec![7u8; 4000]);
        let v = json!({"image_url": {"url": format!("data:image/png;base64,{payload}")}, "text": "short"});
        let mut stored = Vec::new();
        let out = redact_blobs(&v, &mut |b, m| {
            stored.push((b.len(), m.to_string()));
            BlobRef::for_bytes(b, m)
        });
        assert_eq!(stored, vec![(4000, "image/png".to_string())]);
        let s = out["image_url"]["url"].as_str().unwrap();
        assert!(s.starts_with("data:image/png;base64,<blob sha256:"), "{s}");
        assert_eq!(out["text"], "short");
    }
}
