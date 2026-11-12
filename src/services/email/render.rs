//! Template rendering (minijinja) and validation.
//!
//! An email = subject + HTML body + text body, each a minijinja template stored in
//! `email_template_translations`. The HTML body is autoescaped and wrapped in the shared layout
//! (`templates/layout.html`); the text body is generated from the HTML when left empty.
//!
//! Available inside every template:
//! - the variables listed in `email_templates.variables` (anything else is rejected), plus
//!   `dir`, `lang`, `start`, `end` provided by the renderer for bidirectional layouts;
//! - `money(cents, currency)`, `date(value)`, `datetime(value)`, `time(value)` — localized,
//!   in Africa/Casablanca time; `button(url, label)` — a call-to-action button;
//! - partials: `{% include "booking_summary.html" %}` (needs `booking`, `lines`) and
//!   `{% include "cancellation_policy.html" %}` (needs `policy`).
//!
//! Rendering is strict: an unknown variable or attribute is an error, never an empty string.

use std::collections::{BTreeSet, HashSet};

use minijinja::{AutoEscape, Environment, Error, ErrorKind, UndefinedBehavior, Value, context};
use serde_json::Value as Json;

use super::{
    format::{self, Moment},
    labels,
};

const LAYOUT: &str = include_str!("templates/layout.html");
/// `(name, source)` of the partials templates may include.
const PARTIALS: [(&str, &str); 2] = [
    (
        "booking_summary.html",
        include_str!("templates/booking_summary.html"),
    ),
    (
        "cancellation_policy.html",
        include_str!("templates/cancellation_policy.html"),
    ),
];
/// Helpers and builtins: never "unknown variables".
const GLOBALS: [&str; 13] = [
    "money",
    "date",
    "datetime",
    "time",
    "button",
    "range",
    "dict",
    "debug",
    "namespace",
    "loop",
    "caller",
    "varargs",
    "kwargs",
];
/// Always provided by the renderer: `dir` (`ltr`/`rtl`), `lang`, `start`/`end` (`left`/`right`,
/// mirrored for RTL, for `text-align`), and `labels` (wording of the partials).
const RENDERER_VARS: [&str; 5] = ["labels", "dir", "lang", "start", "end"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("{part}: syntax error: {message}")]
    Syntax { part: &'static str, message: String },
    #[error("{part}: unknown variable `{name}` (allowed: {allowed})")]
    UnknownVariable {
        part: &'static str,
        name: String,
        allowed: String,
    },
    #[error("{part}: `{partial}` needs the variable `{name}`")]
    PartialNeeds {
        part: &'static str,
        partial: &'static str,
        name: String,
    },
    #[error("{part}: {message}")]
    Render { part: &'static str, message: String },
}

/// One template translation to render.
pub struct TemplateSource<'a> {
    pub subject: &'a str,
    pub body_html: &'a str,
    /// Empty = generated from the HTML.
    pub body_text: &'a str,
    /// `email_templates.variables`.
    pub variables: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Rendered {
    pub subject: String,
    pub html: String,
    pub text: String,
}

/// Checks syntax and that every placeholder is a declared variable. Run on save and before
/// every send.
pub fn validate(source: &TemplateSource<'_>) -> Result<(), RenderError> {
    let env = environment("en", false);
    let allowed: BTreeSet<&str> = source.variables.iter().map(String::as_str).collect();
    for (part, name, text) in parts(source) {
        if text.is_empty() {
            continue;
        }
        let template =
            env.template_from_named_str(name, text)
                .map_err(|e| RenderError::Syntax {
                    part,
                    message: describe(&e),
                })?;
        let mut used: Vec<String> = template.undeclared_variables(false).into_iter().collect();
        used.sort();
        if let Some(unknown) = used.iter().find(|v| {
            !allowed.contains(v.as_str())
                && !GLOBALS.contains(&v.as_str())
                && !RENDERER_VARS.contains(&v.as_str())
        }) {
            return Err(RenderError::UnknownVariable {
                part,
                name: unknown.clone(),
                allowed: allowed.iter().copied().collect::<Vec<_>>().join(", "),
            });
        }
        for (partial, partial_src) in PARTIALS {
            if !text.contains(partial) {
                continue;
            }
            let needed = env
                .template_from_str(partial_src)
                .expect("built-in partials are valid")
                .undeclared_variables(false);
            if let Some(missing) = sorted(needed).into_iter().find(|v| {
                !allowed.contains(v.as_str())
                    && !GLOBALS.contains(&v.as_str())
                    && !RENDERER_VARS.contains(&v.as_str())
            }) {
                return Err(RenderError::PartialNeeds {
                    part,
                    partial,
                    name: missing,
                });
            }
        }
    }
    Ok(())
}

/// Renders one email. `data` holds the template variables (a JSON object).
pub fn render(
    source: &TemplateSource<'_>,
    data: &Json,
    locale: &str,
    rtl: bool,
) -> Result<Rendered, RenderError> {
    validate(source)?;
    let env = environment(locale, true);
    let (start, end) = if rtl {
        ("right", "left")
    } else {
        ("left", "right")
    };
    // Renderer variables win over data with the same name.
    let ctx = context! {
        labels => Value::from_serialize(labels::labels(locale)),
        dir => if rtl { "rtl" } else { "ltr" },
        lang => locale,
        start => start,
        end => end,
        ..Value::from_serialize(data)
    };

    let render_part = |part: &'static str, name: &str, text: &str| {
        env.template_from_named_str(name, text)
            .and_then(|t| t.render(&ctx))
            .map_err(|e| RenderError::Render {
                part,
                message: describe(&e),
            })
    };
    let subject = render_part("subject", "subject.txt", source.subject)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let body = render_part("html", "body.html", source.body_html)?;
    let text = if source.body_text.trim().is_empty() {
        let site = data.get("site").cloned().unwrap_or(Json::Null);
        format!("{}\n\n{}", html_to_text(&body), text_footer(&site))
    } else {
        render_part("text", "body.txt", source.body_text)?
    };
    let html = env
        .template_from_named_str("layout.html", LAYOUT)
        .and_then(|t| {
            t.render(context! {
                subject => &subject,
                body => Value::from_safe_string(body.clone()),
                ..ctx.clone()
            })
        })
        .map_err(|e| RenderError::Render {
            part: "layout",
            message: describe(&e),
        })?;
    Ok(Rendered {
        subject,
        html,
        text: text.trim().to_string() + "\n",
    })
}

fn parts<'a>(s: &TemplateSource<'a>) -> [(&'static str, &'static str, &'a str); 3] {
    [
        ("subject", "subject.txt", s.subject),
        ("html", "body.html", s.body_html),
        ("text", "body.txt", s.body_text),
    ]
}

fn environment(locale: &str, strict: bool) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|name| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });
    if strict {
        env.set_undefined_behavior(UndefinedBehavior::Strict);
    }
    for (name, source) in PARTIALS {
        env.add_template(name, source)
            .expect("built-in partials are valid");
    }
    let loc = locale.to_string();
    env.add_function("money", move |cents: i64, currency: String| {
        format::money(cents, &currency, &loc)
    });
    let loc = locale.to_string();
    env.add_function("date", move |value: String| -> Result<String, Error> {
        Ok(match moment(&value)? {
            Moment::Day(day) => format::date(day, &loc),
            Moment::Instant(at) => format::date(crate::camp_time::today(at), &loc),
        })
    });
    let loc = locale.to_string();
    env.add_function("datetime", move |value: String| -> Result<String, Error> {
        Ok(match moment(&value)? {
            Moment::Day(day) => format::date(day, &loc),
            Moment::Instant(at) => format::datetime(at, &loc),
        })
    });
    env.add_function("time", |value: String| -> Result<String, Error> {
        match moment(&value)? {
            Moment::Instant(at) => Ok(format::time(at)),
            Moment::Day(_) => Err(Error::new(
                ErrorKind::InvalidOperation,
                "time() needs a timestamp",
            )),
        }
    });
    env.add_function("button", |url: String, label: String| {
        Value::from_safe_string(format!(
            r##"<table role="presentation" cellpadding="0" cellspacing="0" border="0" style="margin:24px auto;"><tr><td align="center" bgcolor="#c0582f" style="border-radius:6px;"><a href="{}" style="display:inline-block;padding:12px 28px;font-family:Arial,Helvetica,sans-serif;font-size:15px;font-weight:bold;color:#ffffff;text-decoration:none;border-radius:6px;">{}</a></td></tr></table>"##,
            escape_html(&url),
            escape_html(&label)
        ))
    });
    env
}

fn moment(value: &str) -> Result<Moment, Error> {
    format::parse_moment(value).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("not a date or timestamp: {value:?}"),
        )
    })
}

/// One line per error in the chain: an error inside an included partial is the source of the
/// "error in <partial>" reported at the include line, and is the part that says what is wrong.
fn describe(e: &Error) -> String {
    let one = |e: &Error| match e.line() {
        Some(line) => format!(
            "{} (line {line})",
            e.detail().unwrap_or(&e.kind().to_string())
        ),
        None => e.to_string(),
    };
    let mut out = one(e);
    let mut cause = std::error::Error::source(e);
    while let Some(inner) = cause {
        match inner.downcast_ref::<Error>() {
            Some(inner) => out.push_str(&format!(": {}", one(inner))),
            None => out.push_str(&format!(": {inner}")),
        }
        cause = inner.source();
    }
    out
}

fn sorted(set: HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn text_footer(site: &Json) -> String {
    let field = |k: &str| site.get(k).and_then(Json::as_str).filter(|s| !s.is_empty());
    let mut lines = vec!["--".to_string()];
    lines.extend(
        ["name", "address", "phone", "whatsapp", "email", "url"]
            .iter()
            .filter_map(|k| field(k).map(str::to_string)),
    );
    lines.join("\n")
}

/// Plain-text version of an email body: paragraphs and table rows become lines, links keep
/// their URL, entities are decoded.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    let mut href_stack: Vec<Option<String>> = Vec::new();
    let mut skip_depth = 0usize;
    while let Some(lt) = rest.find('<') {
        if skip_depth == 0 {
            push_text(&mut out, &rest[..lt]);
        }
        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("style" | "script" | "head" | "title", false) => skip_depth += 1,
            ("style" | "script" | "head" | "title", true) => {
                skip_depth = skip_depth.saturating_sub(1)
            }
            ("br", _) => out.push('\n'),
            ("p" | "div" | "table" | "h1" | "h2" | "h3" | "h4" | "ul" | "ol", _) => {
                out.push_str("\n\n")
            }
            ("tr", true) => out.push('\n'),
            ("li", false) => out.push_str("\n- "),
            ("td" | "th", true) => out.push_str("  "),
            ("a", false) => href_stack.push(attr(tag, "href")),
            ("a", true) => {
                if let Some(Some(href)) = href_stack.pop() {
                    let label_is_url = out.trim_end().ends_with(&href);
                    if !label_is_url && !href.starts_with("mailto:") && !href.starts_with("tel:") {
                        out.push_str(&format!(" ({href})"));
                    }
                }
            }
            _ => {}
        }
    }
    if skip_depth == 0 {
        push_text(&mut out, rest);
    }
    // Tidy: trim each line, at most one blank line in a row.
    let mut tidy = String::new();
    let mut blank = 0;
    for line in out.lines().map(str::trim) {
        if line.is_empty() {
            blank += 1;
            if blank > 1 || tidy.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim().to_string()
}

fn push_text(out: &mut String, raw: &str) {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        if raw.chars().any(char::is_whitespace) && !out.ends_with([' ', '\n']) {
            out.push(' ');
        }
        return;
    }
    if raw.starts_with(char::is_whitespace) && !out.ends_with([' ', '\n']) {
        out.push(' ');
    }
    out.push_str(&decode_entities(&collapsed));
    if raw.ends_with(char::is_whitespace) {
        out.push(' ');
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(decode_entities(&tag[start..end]))
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_variables_are_rejected_on_every_part() {
        let variables = vars(&["guest", "site"]);
        let ok = TemplateSource {
            subject: "Bonjour {{ guest.first_name }}",
            body_html: "<p>{{ money(1200, 'EUR') }} {{ site.name }}</p>{% for x in range(2) %}{{ loop.index }}{% endfor %}",
            body_text: "",
            variables: &variables,
        };
        assert_eq!(validate(&ok), Ok(()));

        let bad = TemplateSource {
            body_html: "<p>{{ bookng.reference }}</p>",
            ..ok
        };
        assert!(matches!(
            validate(&bad),
            Err(RenderError::UnknownVariable { part: "html", ref name, .. }) if name == "bookng"
        ));
        let bad = TemplateSource {
            subject: "{{ password }}",
            ..ok
        };
        assert!(matches!(
            validate(&bad),
            Err(RenderError::UnknownVariable {
                part: "subject",
                ..
            })
        ));
        let bad = TemplateSource {
            body_html: "<p>{% if %}</p>",
            ..ok
        };
        assert!(matches!(validate(&bad), Err(RenderError::Syntax { .. })));
    }

    #[test]
    fn partials_require_their_variables() {
        let variables = vars(&["booking", "site"]);
        let source = TemplateSource {
            subject: "x",
            body_html: r#"{% include "booking_summary.html" %}"#,
            body_text: "",
            variables: &variables,
        };
        assert!(matches!(
            validate(&source),
            Err(RenderError::PartialNeeds { partial: "booking_summary.html", ref name, .. }) if name == "lines"
        ));
    }

    #[test]
    fn html_is_escaped_layout_wraps_and_text_is_generated() {
        let variables = vars(&["guest", "site"]);
        let source = TemplateSource {
            subject: "Hello {{ guest.first_name }}",
            body_html: "<p>Hello {{ guest.first_name }},</p><p>See <a href=\"https://mouzoun.com/b\">your booking</a>.</p>{{ button('https://mouzoun.com/b', 'Open') }}",
            body_text: "",
            variables: &variables,
        };
        let data = json!({
            "guest": {"first_name": "<script>x</script>"},
            "site": {"name": "Mouzoun", "url": "https://mouzoun.com", "logo_url": null, "tagline": null,
                     "address": "M'ssici", "phone": "+212600000000", "whatsapp": null, "whatsapp_url": null, "email": null},
        });
        let out = render(&source, &data, "ar", true).unwrap();
        assert!(out.html.contains(r#"dir="rtl""#) && out.html.contains(r#"lang="ar""#));
        assert!(out.html.contains("&lt;script&gt;"), "autoescaped");
        assert!(!out.html.contains("<script>"));
        assert_eq!(
            out.subject, "Hello <script>x</script>",
            "subjects are plain text"
        );
        assert!(
            out.text
                .contains("See your booking (https://mouzoun.com/b)."),
            "{}",
            out.text
        );
        assert!(out.text.contains("M'ssici"), "footer in the text version");

        // Strict: a missing attribute is an error, not an empty string.
        let missing = render(
            &source,
            &json!({"guest": {}, "site": data["site"]}),
            "fr",
            false,
        );
        assert!(matches!(
            missing,
            Err(RenderError::Render {
                part: "subject",
                ..
            })
        ));
    }

    #[test]
    fn html_to_text_keeps_structure() {
        let text = html_to_text(
            "<h1>Title</h1><p>Line one<br>Line two &amp; more</p><ul><li>A</li><li>B</li></ul><table><tr><td>Total</td><td>120,00&nbsp;€</td></tr></table><style>p{}</style>",
        );
        assert_eq!(
            text,
            "Title\n\nLine one\nLine two & more\n\n- A\n- B\n\nTotal  120,00\u{a0}€"
        );
    }
}
