//! Public paths as the redirect layer compares them: percent-decoded, Unicode NFC (Arabic
//! slugs), no trailing slash (except `/`), no double slashes. Case is kept (slugs may be
//! case-sensitive, Arabic has none). The Nuxt middleware applies the same rule
//! (`front/server/utils/seo-path.ts`).

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use unicode_normalization::UnicodeNormalization;

/// Characters escaped in sitemap URLs (RFC 3986 path segment, plus non-ASCII as UTF-8).
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'\'');

/// The normalised form of a request path (no query string). `None` when it is not a path.
pub fn normalize(raw: &str) -> Option<String> {
    let raw = raw.split(['?', '#']).next().unwrap_or("");
    if !raw.starts_with('/') {
        return None;
    }
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_or_else(|_| raw.to_string(), |c| c.into_owned());
    let nfc: String = decoded.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    for c in nfc.chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    Some(out)
}

/// Percent-encodes a normalised path for an absolute URL (sitemaps, Location headers).
pub fn encode(path: &str) -> String {
    utf8_percent_encode(path, PATH).to_string()
}

/// `base` (no trailing slash) + encoded path.
pub fn absolute(base: &str, path: &str) -> String {
    if path.starts_with("https://") || path.starts_with("http://") {
        return path.to_string();
    }
    let base = base.trim_end_matches('/');
    if path == "/" {
        format!("{base}/")
    } else {
        format!("{base}{}", encode(path))
    }
}

/// Requests that are never worth logging as 404 (scanners).
pub fn is_noise(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.len() > 300
        || p.ends_with(".php")
        || p.contains(".php/")
        || p.starts_with("/wp-")
        || p.contains("/wp-")
        || p.contains("/.env")
        || p.ends_with(".env")
        || p.contains("/.git")
        || p.starts_with("/cgi-bin")
        || p.contains("xmlrpc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_like_the_front() {
        assert_eq!(normalize("/boutique/").as_deref(), Some("/boutique"));
        assert_eq!(
            normalize("//boutique//dattes/").as_deref(),
            Some("/boutique/dattes")
        );
        assert_eq!(normalize("/").as_deref(), Some("/"));
        assert_eq!(normalize("/a?utm=1").as_deref(), Some("/a"));
        assert_eq!(
            normalize("/ar/boutique/%D8%AA%D9%85%D9%88%D8%B1").as_deref(),
            Some("/ar/boutique/تمور")
        );
        // NFD "é" (e + combining acute) becomes NFC.
        assert_eq!(
            normalize("/e\u{301}t\u{e9}").as_deref(),
            Some("/\u{e9}t\u{e9}")
        );
        assert_eq!(normalize("/Dattes").as_deref(), Some("/Dattes"));
        assert_eq!(normalize("boutique"), None);
    }

    #[test]
    fn encodes_for_absolute_urls() {
        assert_eq!(
            absolute("https://mouzoun.com/", "/ar/boutique/تمور"),
            "https://mouzoun.com/ar/boutique/%D8%AA%D9%85%D9%88%D8%B1"
        );
        assert_eq!(absolute("https://mouzoun.com", "/"), "https://mouzoun.com/");
        assert_eq!(
            absolute("https://x", "https://ext.example/a"),
            "https://ext.example/a"
        );
    }

    #[test]
    fn recognises_scanner_noise() {
        for p in [
            "/wp-login.php",
            "/.env",
            "/.git/config",
            "/admin.php",
            "/x/wp-admin",
        ] {
            assert!(is_noise(p), "{p}");
        }
        assert!(!is_noise("/boutique/dattes"));
        assert!(is_noise(&format!("/{}", "a".repeat(300))));
    }
}
