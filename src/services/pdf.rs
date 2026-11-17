//! A minimal PDF writer for text documents (invoices, terms of sale, the withdrawal form):
//! A4 pages, the standard Helvetica fonts (no embedding), WinAnsi encoding (French and the other
//! Latin languages; characters outside it print as `?`, Arabic included: documents for consumers
//! are in French, which the law requires anyway). Long lines wrap; pages break automatically.
//! The output is deterministic for the same input, so its SHA-256 identifies the document.

use sha2::{Digest, Sha256};

const PAGE_W: f32 = 595.0;
const PAGE_H: f32 = 842.0;
const MARGIN: f32 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Title,
    Heading,
    Body,
    Small,
    /// Body text aligned to the right edge (amounts).
    Right,
}

impl Style {
    fn size(self) -> f32 {
        match self {
            Self::Title => 16.0,
            Self::Heading => 11.5,
            Self::Body | Self::Right => 9.5,
            Self::Small => 7.5,
        }
    }
    fn bold(self) -> bool {
        matches!(self, Self::Title | Self::Heading)
    }
}

/// A document being written: a list of styled paragraphs and table rows.
#[derive(Debug, Default, Clone)]
pub struct Document {
    blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
enum Block {
    Text(Style, String),
    /// Columns with relative widths (sum ≤ 1); the last column is right-aligned.
    Row(Vec<(f32, String)>, bool),
    Space(f32),
    Rule,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn text(&mut self, style: Style, text: impl Into<String>) -> &mut Self {
        self.blocks.push(Block::Text(style, text.into()));
        self
    }
    pub fn row(&mut self, cols: &[(f32, &str)], bold: bool) -> &mut Self {
        self.blocks.push(Block::Row(
            cols.iter().map(|(w, t)| (*w, (*t).to_string())).collect(),
            bold,
        ));
        self
    }
    pub fn space(&mut self, points: f32) -> &mut Self {
        self.blocks.push(Block::Space(points));
        self
    }
    pub fn rule(&mut self) -> &mut Self {
        self.blocks.push(Block::Rule);
        self
    }

    /// Renders the PDF bytes.
    pub fn render(&self) -> Vec<u8> {
        let mut pages: Vec<String> = vec![String::new()];
        let mut y = PAGE_H - MARGIN;
        let width = PAGE_W - 2.0 * MARGIN;
        let need = |pages: &mut Vec<String>, y: &mut f32, h: f32| {
            if *y - h < MARGIN {
                pages.push(String::new());
                *y = PAGE_H - MARGIN;
            }
        };
        for block in &self.blocks {
            match block {
                Block::Space(h) => y -= h,
                Block::Rule => {
                    need(&mut pages, &mut y, 6.0);
                    let page = pages.last_mut().expect("one page");
                    page.push_str(&format!(
                        "0.6 w {MARGIN} {:.1} m {:.1} {:.1} l S\n",
                        y - 2.0,
                        PAGE_W - MARGIN,
                        y - 2.0
                    ));
                    y -= 8.0;
                }
                Block::Text(style, text) => {
                    let size = style.size();
                    let lead = size * 1.35;
                    for paragraph in text.split('\n') {
                        for line in wrap(paragraph, width, size, style.bold()) {
                            need(&mut pages, &mut y, lead);
                            y -= lead;
                            let x = if *style == Style::Right {
                                PAGE_W - MARGIN - text_width(&line, size, false)
                            } else {
                                MARGIN
                            };
                            pages.last_mut().expect("one page").push_str(&show(
                                &line,
                                x,
                                y,
                                size,
                                style.bold(),
                            ));
                        }
                    }
                    if style.bold() {
                        y -= 2.0;
                    }
                }
                Block::Row(cols, bold) => {
                    let size = Style::Body.size();
                    let lead = size * 1.35;
                    // Wrap each cell to its width; the row is as tall as its tallest cell.
                    let wrapped: Vec<Vec<String>> = cols
                        .iter()
                        .map(|(w, t)| wrap(t, width * w - 6.0, size, *bold))
                        .collect();
                    let lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
                    need(&mut pages, &mut y, lead * lines as f32);
                    let page = pages.last_mut().expect("one page");
                    let mut x = MARGIN;
                    let last = cols.len().saturating_sub(1);
                    for (i, ((w, _), cell)) in cols.iter().zip(&wrapped).enumerate() {
                        for (n, line) in cell.iter().enumerate() {
                            let ly = y - lead * (n as f32 + 1.0);
                            let lx = if i == last && i > 0 {
                                x + width * w - text_width(line, size, *bold)
                            } else {
                                x
                            };
                            page.push_str(&show(line, lx, ly, size, *bold));
                        }
                        x += width * w;
                    }
                    y -= lead * lines as f32 + 2.0;
                }
            }
        }
        assemble(&pages)
    }
}

/// SHA-256 (hex) of bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A character in WinAnsiEncoding (cp1252), `?` when it has none.
fn win_ansi(c: char) -> u8 {
    match c {
        '\u{20}'..='\u{7e}' => c as u8,
        '\u{a0}'..='\u{ff}' => c as u32 as u8,
        '€' => 0x80,
        '‚' => 0x82,
        '„' => 0x84,
        '…' => 0x85,
        '‰' => 0x89,
        'Š' => 0x8a,
        '‹' => 0x8b,
        'Œ' => 0x8c,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '™' => 0x99,
        'š' => 0x9a,
        '›' => 0x9b,
        'œ' => 0x9c,
        'Ÿ' => 0x9f,
        '\u{202f}' | '\u{2009}' => b' ',
        _ => b'?',
    }
}

/// Approximate Helvetica advance widths (per 1000 em) for wrapping and right alignment.
fn char_width(c: char, bold: bool) -> f32 {
    let w = match c {
        'i' | 'j' | 'l' | '.' | ',' | ':' | ';' | '\'' | '|' | '!' => 260.0,
        'f' | 't' | 'r' | 'I' | ' ' | '(' | ')' | '-' | '/' => 320.0,
        'm' | 'M' | 'W' | 'w' | '@' | '%' => 830.0,
        'A'..='Z' => 680.0,
        '0'..='9' => 556.0,
        _ => 540.0,
    };
    if bold { w * 1.06 } else { w }
}

fn text_width(s: &str, size: f32, bold: bool) -> f32 {
    s.chars().map(|c| char_width(c, bold)).sum::<f32>() * size / 1000.0
}

fn wrap(text: &str, width: f32, size: f32, bold: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if text_width(&candidate, size, bold) <= width || line.is_empty() {
            line = candidate;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    lines.push(line);
    lines
}

fn escape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push(b'\\');
                out.push(c as u8);
            }
            _ => out.push(win_ansi(c)),
        }
    }
    out
}

fn show(text: &str, x: f32, y: f32, size: f32, bold: bool) -> String {
    // Stream content is bytes: non-ASCII WinAnsi codes are written as octal escapes.
    let encoded: String = escape(text)
        .into_iter()
        .map(|b| {
            if b.is_ascii() && b >= 0x20 {
                (b as char).to_string()
            } else {
                format!("\\{b:03o}")
            }
        })
        .collect();
    format!(
        "BT /{} {size:.1} Tf {x:.1} {y:.1} Td ({encoded}) Tj ET\n",
        if bold { "F2" } else { "F1" }
    )
}

fn assemble(pages: &[String]) -> Vec<u8> {
    // Objects: 1 catalog, 2 pages, 3 F1, 4 F2, then (page, content) per page.
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        String::new(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
            .into(),
    ];
    let mut kids = Vec::new();
    for (i, content) in pages.iter().enumerate() {
        let page_obj = 5 + i * 2;
        kids.push(format!("{page_obj} 0 R"));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
            page_obj + 1
        ));
        let footer = format!(
            "BT /F1 7 Tf {:.1} 25 Td ({} / {}) Tj ET\n",
            PAGE_W - MARGIN - 20.0,
            i + 1,
            pages.len()
        );
        let stream = format!("{content}{footer}");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}endstream",
            stream.len()
        ));
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    );
    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{obj}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Plain text of HTML (paragraphs and list items on their own lines), for PDF documents.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match (c, in_tag) {
            ('<', _) => {
                in_tag = true;
                tag.clear();
            }
            ('>', true) => {
                in_tag = false;
                let t = tag
                    .trim_start_matches('/')
                    .split(' ')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    t.as_str(),
                    "p" | "br" | "li" | "h1" | "h2" | "h3" | "h4" | "div" | "tr"
                ) && !out.ends_with('\n')
                {
                    out.push('\n');
                }
                if t == "li" && !tag.starts_with('/') {
                    out.push_str("• ");
                }
            }
            (c, true) => tag.push(c),
            (c, false) => out.push(c),
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pdf_is_produced_deterministically() {
        let mut d = Document::new();
        d.text(Style::Title, "Facture F2026-000001").text(
            Style::Body,
            "Prix TTC : 12,50 € (TVA 5,5 %) — œuvre « test » (paren)",
        );
        d.row(&[(0.7, "Dattes Medjool 500 g"), (0.3, "12,00 €")], false);
        for _ in 0..120 {
            d.text(
                Style::Body,
                "Ligne longue pour forcer un saut de page et vérifier la pagination.",
            );
        }
        let a = d.render();
        assert!(a.starts_with(b"%PDF-1.4"));
        assert!(a.ends_with(b"%%EOF\n"));
        assert_eq!(sha256_hex(&a), sha256_hex(&d.render()));
        let text = String::from_utf8_lossy(&a);
        assert!(
            text.contains("/Count 3") || text.contains("/Count 2"),
            "several pages"
        );
        assert!(text.contains("\\200"), "euro sign in WinAnsi");
        assert!(text.contains("\\(paren\\)"));
    }

    #[test]
    fn html_becomes_lines() {
        assert_eq!(
            html_to_text(
                "<h2>Article 1</h2><p>Le vendeur &amp; l'acheteur.</p><ul><li>A</li></ul>"
            )
            .trim(),
            "Article 1\nLe vendeur & l'acheteur.\n• A"
        );
    }
}
