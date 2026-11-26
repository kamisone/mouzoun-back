//! Upload policy: what may be uploaded and where it is stored. Pure functions, unit-tested.

use chrono::{DateTime, Datelike, Utc};
use uuid::Uuid;

/// Public objects have unique, never-reused keys, so they can be cached forever.
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Private files: never kept by shared caches (CDN, proxies) nor reused from the browser cache
/// once their signed URL has expired.
pub const PRIVATE_CACHE_CONTROL: &str = "private, no-store";

#[derive(Debug)]
pub struct FileKind {
    pub content_type: &'static str,
    pub extension: &'static str,
    pub is_image: bool,
    /// Videos have their own size limit (`MEDIA_MAX_VIDEO_BYTES`).
    pub is_video: bool,
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
        is_video: false,
        magic: |b| b.starts_with(&[0xFF, 0xD8, 0xFF]),
    },
    FileKind {
        content_type: "image/png",
        extension: "png",
        is_image: true,
        is_video: false,
        magic: |b| b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
    },
    FileKind {
        content_type: "image/webp",
        extension: "webp",
        is_image: true,
        is_video: false,
        magic: |b| b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP",
    },
    FileKind {
        content_type: "image/avif",
        extension: "avif",
        is_image: true,
        is_video: false,
        magic: |b| {
            b.len() >= 12 && &b[4..8] == b"ftyp" && (&b[8..12] == b"avif" || &b[8..12] == b"avis")
        },
    },
    FileKind {
        content_type: "application/pdf",
        extension: "pdf",
        is_image: false,
        is_video: false,
        magic: |b| b.starts_with(b"%PDF-"),
    },
    FileKind {
        content_type: "video/mp4",
        extension: "mp4",
        is_image: false,
        is_video: true,
        magic: is_mp4,
    },
    FileKind {
        content_type: "video/webm",
        extension: "webm",
        is_image: false,
        is_video: true,
        magic: is_webm,
    },
];

/// ISO base media file (`ftyp` box) with an MP4 brand. QuickTime (`qt  `) and image brands
/// (AVIF, HEIC) are not MP4. The codec (H.264 recommended) is not checked: browsers play H.264
/// and H.265 MP4s alike on most devices, and the editor asks for H.264.
fn is_mp4(b: &[u8]) -> bool {
    const BRANDS: [&[u8; 4]; 12] = [
        b"isom", b"iso2", b"iso4", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1", b"dash", b"MSNV",
        b"M4V ", b"mmp4",
    ];
    b.len() >= 12 && &b[4..8] == b"ftyp" && BRANDS.iter().any(|brand| &b[8..12] == *brand)
}

/// Matroska / EBML header whose doctype is `webm`.
fn is_webm(b: &[u8]) -> bool {
    b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
        && b[..b.len().min(64)].windows(4).any(|w| w == b"webm")
}

/// A QuickTime file (`.mov`, what iPhones record): refused with its own message (export as MP4).
pub fn is_quicktime(content_type: &str, head: &[u8]) -> bool {
    content_type.trim().eq_ignore_ascii_case("video/quicktime")
        || (head.len() >= 12 && &head[4..8] == b"ftyp" && &head[8..12] == b"qt  ")
}

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
    fn videos_are_mp4_or_webm_never_quicktime() {
        let mp4 = kind_for("video/mp4").unwrap();
        assert!(mp4.is_video);
        assert!(mp4.matches(b"\0\0\0\x20ftypisom\0\0\x02\0isomiso2avc1mp41"));
        assert!(mp4.matches(b"\0\0\0\x18ftypmp42\0\0\0\0"));
        assert!(
            !mp4.matches(b"\0\0\0\x14ftypqt  \0\0\0\0"),
            "a .mov is not an MP4"
        );
        assert!(
            !mp4.matches(b"\0\0\0\x1cftypavif\0\0\0\0"),
            "an AVIF image is not an MP4"
        );
        assert!(!mp4.matches(b"<html>"));
        let webm = kind_for("video/webm").unwrap();
        assert!(webm.matches(
            b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01\x42\xf7\x81\x01\x42\x82\x84webm\x42\x87"
        ));
        assert!(
            !webm.matches(b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01\x42\x82\x88matroska"),
            "plain MKV is not WebM"
        );
        assert!(is_quicktime("video/quicktime", b""));
        assert!(is_quicktime("video/mp4", b"\0\0\0\x14ftypqt  \0\0\0\0"));
        assert!(!is_quicktime("video/mp4", b"\0\0\0\x18ftypmp42\0\0\0\0"));
        assert!(kind_for("video/quicktime").is_none());
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
