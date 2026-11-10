//! Upload policy: what may be uploaded and where it is stored. Pure functions, unit-tested.

use chrono::{DateTime, Datelike, Utc};
use uuid::Uuid;

/// Bytes read back from storage to verify the real file type.
pub const SNIFF_LEN: u64 = 16;

/// Public objects have unique, never-reused keys, so they can be cached forever.
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

#[derive(Debug)]
pub struct FileKind {
    pub content_type: &'static str,
    pub extension: &'static str,
    pub is_image: bool,
    magic: fn(&[u8]) -> bool,
}

impl FileKind {
    /// True if the leading bytes match the declared format.
    pub fn matches(&self, head: &[u8]) -> bool {
        (self.magic)(head)
    }
}

const ALLOWED: &[FileKind] = &[
    FileKind {
        content_type: "image/jpeg",
        extension: "jpg",
        is_image: true,
        magic: |b| b.starts_with(&[0xFF, 0xD8, 0xFF]),
    },
    FileKind {
        content_type: "image/png",
        extension: "png",
        is_image: true,
        magic: |b| b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
    },
    FileKind {
        content_type: "image/webp",
        extension: "webp",
        is_image: true,
        magic: |b| b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP",
    },
    FileKind {
        content_type: "image/avif",
        extension: "avif",
        is_image: true,
        magic: |b| {
            b.len() >= 12 && &b[4..8] == b"ftyp" && (&b[8..12] == b"avif" || &b[8..12] == b"avis")
        },
    },
    FileKind {
        content_type: "application/pdf",
        extension: "pdf",
        is_image: false,
        magic: |b| b.starts_with(b"%PDF-"),
    },
];

pub fn kind_for(content_type: &str) -> Option<&'static FileKind> {
    let normalized = content_type.trim().to_ascii_lowercase();
    ALLOWED.iter().find(|k| k.content_type == normalized)
}

pub fn allowed_content_types() -> Vec<&'static str> {
    ALLOWED.iter().map(|k| k.content_type).collect()
}

/// Storage key: `media/YYYY/MM/<uuid>.<ext>`. Never derived from user input
/// (no path traversal, no collisions, no information leak through filenames).
pub fn object_key(id: Uuid, kind: &FileKind, now: DateTime<Utc>) -> String {
    format!(
        "media/{:04}/{:02}/{id}.{}",
        now.year(),
        now.month(),
        kind.extension
    )
}

/// Keeps a display-safe version of the client filename (stored for the admin UI and
/// `Content-Disposition` on download).
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '<' | '>' | '|' | '*' | '?' | ':'))
        .collect::<String>()
        .trim()
        .chars()
        .take(200)
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "file".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn allowlist_is_case_insensitive_and_strict() {
        assert!(kind_for("IMAGE/JPEG").is_some());
        assert!(kind_for("image/svg+xml").is_none(), "SVG can carry scripts");
        assert!(kind_for("text/html").is_none());
    }

    #[test]
    fn magic_bytes_are_verified() {
        let png = kind_for("image/png").unwrap();
        assert!(png.matches(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(!png.matches(b"<html><script>"));
        let webp = kind_for("image/webp").unwrap();
        assert!(webp.matches(b"RIFF\x24\0\0\0WEBPVP8 "));
        assert!(!webp.matches(b"RIFF"));
        let avif = kind_for("image/avif").unwrap();
        assert!(avif.matches(b"\0\0\0\x1cftypavif\0\0\0\0"));
        let pdf = kind_for("application/pdf").unwrap();
        assert!(pdf.matches(b"%PDF-1.7\n"));
    }

    #[test]
    fn object_keys_are_partitioned_and_opaque() {
        let id = Uuid::nil();
        let now = Utc.with_ymd_and_hms(2026, 3, 9, 0, 0, 0).unwrap();
        let key = object_key(id, kind_for("image/jpeg").unwrap(), now);
        assert_eq!(
            key,
            "media/2026/03/00000000-0000-0000-0000-000000000000.jpg"
        );
    }

    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename("C:\\Users\\me\\photo dune.JPG"),
            "photo dune.JPG"
        );
        assert_eq!(sanitize_filename("a\"b<c>.png"), "abc.png");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename(&"x".repeat(500)).len(), 200);
    }
}
