//! Machine translation of admin-written content (the "Générer" buttons of the back office),
//! modelled on the vitecamio project: the admin writes the source language (French, the default
//! locale), one click fills every other active locale.
//!
//! Provider: Google Translate's free public web endpoint (`translate_a/single?client=gtx`, the
//! one the `translate` npm package uses): no key, no billing, unofficial, may rate-limit or
//! change without notice. Languages are translated one after the other (never a burst of
//! parallel calls); a language that fails comes back empty with a reason, the others are kept.
//!
//! Texts keep their structure: plain text is translated paragraph by paragraph (line breaks
//! kept); HTML is translated block by block (`p`, `h2`, `h3`, `h4`, `li`), inline `strong` /
//! `em` / `b` / `i` swapped for numeric tokens `⟦0⟧…⟦/0⟧` that the translator leaves in place
//! (tag names would be translated: `strong` → `fort`).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::config::TranslateConfig;

#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error("translation is disabled")]
    Disabled,
    #[error("translation service unavailable: {0}")]
    Unavailable(String),
    #[error("translation service returned no text")]
    Empty,
}

#[derive(Clone)]
pub struct Translator {
    http: reqwest::Client,
    config: TranslateConfig,
}

/// Every target locale: the translated fields, or empty fields and a reason in `errors`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Outcome {
    pub result: BTreeMap<String, BTreeMap<String, String>>,
    pub errors: BTreeMap<String, String>,
}

impl Translator {
    pub fn new(http: reqwest::Client, config: TranslateConfig) -> Self {
        Self { http, config }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// One plain string (may hold several lines).
    pub async fn text(&self, text: &str, from: &str, to: &str) -> Result<String, TranslateError> {
        if !self.config.enabled {
            return Err(TranslateError::Disabled);
        }
        let mut out = Vec::new();
        // Paragraph by paragraph: keeps line breaks, and each request stays small.
        for line in text.split('\n') {
            if line.trim().is_empty() {
                out.push(line.to_string());
            } else {
                out.push(self.call(line, from, to).await?);
            }
        }
        Ok(out.join("\n"))
    }

    /// An HTML fragment, block by block, inline tags preserved.
    pub async fn html(&self, html: &str, from: &str, to: &str) -> Result<String, TranslateError> {
        if !html.contains('<') {
            return self.text(html, from, to).await;
        }
        let mut out = String::new();
        let mut rest = html;
        while let Some((before, open_tag, inner, close_tag, after)) = next_block(rest) {
            out.push_str(before);
            out.push_str(open_tag);
            let (tokens, tags) = to_tokens(inner);
            if tokens.trim().is_empty() {
                out.push_str(inner);
            } else {
                let translated = self.call(&tokens, from, to).await?;
                out.push_str(&from_tokens(&translated, &tags));
            }
            out.push_str(close_tag);
            rest = after;
        }
        out.push_str(rest);
        Ok(out)
    }

    async fn call(&self, q: &str, from: &str, to: &str) -> Result<String, TranslateError> {
        let response = self
            .http
            .get(format!("{}/translate_a/single", self.config.api_base))
            .query(&[
                ("client", "gtx"),
                ("sl", from),
                ("tl", to),
                ("dt", "t"),
                ("q", q),
            ])
            .send()
            .await
            .map_err(|e| TranslateError::Unavailable(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(TranslateError::Unavailable(if status.as_u16() == 429 {
                "rate limited, try again in a few minutes".into()
            } else {
                format!("HTTP {status}")
            }));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| TranslateError::Unavailable(e.to_string()))?;
        let text = parse_response(&body);
        if text.trim().is_empty() {
            return Err(TranslateError::Empty);
        }
        // Keep the source's outer spacing (the service trims it).
        let lead = &q[..q.len() - q.trim_start().len()];
        let trail = &q[q.trim_end().len()..];
        Ok(format!("{lead}{}{trail}", text.trim()))
    }

    /// Translates `fields` (key → source text) from `from` into each of `targets`, sequentially.
    /// Keys in `html` are HTML fragments. Empty sources stay empty.
    pub async fn fields(
        &self,
        fields: &BTreeMap<String, String>,
        html: &[String],
        from: &str,
        targets: &[String],
    ) -> Outcome {
        let mut outcome = Outcome::default();
        for to in targets {
            let mut values = BTreeMap::new();
            let mut failed = None;
            for (key, source) in fields {
                if source.trim().is_empty() {
                    values.insert(key.clone(), String::new());
                    continue;
                }
                let r = if html.contains(key) {
                    self.html(source, from, to).await
                } else {
                    self.text(source, from, to).await
                };
                match r {
                    Ok(v) => {
                        values.insert(key.clone(), v);
                    }
                    Err(e) => {
                        tracing::warn!(locale = %to, field = %key, error = %e, "translation failed");
                        failed = Some(e.to_string());
                        break;
                    }
                }
            }
            match failed {
                // A failed language returns empty values: the screen never blanks what exists.
                Some(reason) => {
                    outcome.result.insert(
                        to.clone(),
                        fields.keys().map(|k| (k.clone(), String::new())).collect(),
                    );
                    outcome.errors.insert(to.clone(), reason);
                }
                None => {
                    outcome.result.insert(to.clone(), values);
                }
            }
        }
        outcome
    }
}

/// `[[["Bonjour","Hello",...],["le monde","world",...]], ...]` → "Bonjour le monde".
fn parse_response(body: &Value) -> String {
    body.get(0)
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get(0).and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

const BLOCKS: [&str; 5] = ["p", "h2", "h3", "h4", "li"];
const INLINE: [&str; 4] = ["strong", "em", "b", "i"];

/// The next block element: (before, opening tag, inner HTML, closing tag, after).
fn next_block(s: &str) -> Option<(&str, &str, &str, &str, &str)> {
    let lower = s.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for tag in BLOCKS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(&format!("<{tag}")) {
            let at = from + i;
            let next = lower.as_bytes().get(at + tag.len() + 1).copied();
            if matches!(next, Some(b'>' | b' ' | b'\t' | b'\n')) {
                if best.is_none_or(|(b, _)| at < b) {
                    best = Some((at, tag));
                }
                break;
            }
            from = at + 1;
        }
    }
    let (start, tag) = best?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = format!("</{tag}>");
    let close_at = open_end + lower[open_end..].find(&close)?;
    Some((
        &s[..start],
        &s[start..open_end],
        &s[open_end..close_at],
        &s[close_at..close_at + close.len()],
        &s[close_at + close.len()..],
    ))
}

/// Inline tags → `⟦n⟧…⟦/n⟧`; other tags are kept as they are (rare: links).
fn to_tokens(inner: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut rest = inner;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let Some(gt) = rest[lt..].find('>') else {
            out.push_str(&rest[lt..]);
            return (out, tags);
        };
        let tag = &rest[lt..lt + gt + 1];
        let name = tag
            .trim_start_matches('<')
            .trim_start_matches('/')
            .trim_end_matches('>')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if INLINE.contains(&name.as_str()) {
            if tag.starts_with("</") {
                let n = open.pop().unwrap_or(0);
                out.push_str(&format!("⟦/{n}⟧"));
            } else {
                let n = tags.len();
                tags.push(tag.to_string());
                open.push(n);
                out.push_str(&format!("⟦{n}⟧"));
            }
        } else {
            out.push_str(tag);
        }
        rest = &rest[lt + gt + 1..];
    }
    out.push_str(rest);
    (out, tags)
}

fn from_tokens(text: &str, tags: &[String]) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('⟦') {
        out.push_str(&rest[..i]);
        let after = &rest[i + '⟦'.len_utf8()..];
        let Some(j) = after.find('⟧') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let token = after[..j].trim();
        let (closing, num) = match token.strip_prefix('/') {
            Some(n) => (true, n.trim()),
            None => (false, token),
        };
        match num.parse::<usize>().ok().and_then(|n| tags.get(n)) {
            Some(open) => {
                if closing {
                    let name = open
                        .trim_start_matches('<')
                        .split(|c: char| c == '>' || c.is_whitespace())
                        .next()
                        .unwrap_or("");
                    out.push_str(&format!("</{name}>"));
                } else {
                    out.push_str(open);
                }
            }
            None => out.push_str(&rest[i..i + '⟦'.len_utf8() + j + '⟧'.len_utf8()]),
        }
        rest = &after[j + '⟧'.len_utf8()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_tags_become_numeric_tokens_and_back() {
        let (t, tags) = to_tokens("Une <strong>datte</strong> très <em>douce</em>.");
        assert_eq!(t, "Une ⟦0⟧datte⟦/0⟧ très ⟦1⟧douce⟦/1⟧.");
        assert_eq!(
            from_tokens("A ⟦0⟧date⟦/0⟧ very ⟦1⟧sweet⟦/1⟧.", &tags),
            "A <strong>date</strong> very <em>sweet</em>."
        );
        // Spaces the translator may add inside the brackets.
        assert_eq!(from_tokens("⟦ 0 ⟧x⟦ /0⟧", &tags), "<strong>x</strong>");
    }

    #[test]
    fn blocks_are_found_in_order() {
        let html = "<h2>Titre</h2>\n<p class=\"a\">Un <b>texte</b></p><ul><li>A</li></ul>";
        let (before, open, inner, close, after) = next_block(html).unwrap();
        assert_eq!((before, open, inner, close), ("", "<h2>", "Titre", "</h2>"));
        let (_, open, inner, _, after) = next_block(after).unwrap();
        assert_eq!((open, inner), ("<p class=\"a\">", "Un <b>texte</b>"));
        let (before, _, inner, _, rest) = next_block(after).unwrap();
        assert_eq!((before, inner, rest), ("<ul>", "A", "</ul>"));
        assert!(next_block("<pre>x</pre>").is_none());
    }

    #[test]
    fn the_response_parts_are_joined() {
        let body = serde_json::json!([
            [["Hello ", "Bonjour ", null], ["world", "le monde", null]],
            null,
            "fr"
        ]);
        assert_eq!(parse_response(&body), "Hello world");
    }
}
