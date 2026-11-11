//! Email templates in the database: loading with locale fallback, validated saving.

use sqlx::PgPool;

use super::render::{self, RenderError, TemplateSource};

/// A template translation ready to render.
#[derive(Debug, Clone)]
pub struct LoadedTemplate {
    pub code: String,
    pub variables: Vec<String>,
    /// Locale of the translation actually used (the default one when the wanted is missing).
    pub locale: String,
    pub rtl: bool,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
}

impl LoadedTemplate {
    pub fn source(&self) -> TemplateSource<'_> {
        TemplateSource {
            subject: &self.subject,
            body_html: &self.body_html,
            body_text: &self.body_text,
            variables: &self.variables,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocaleInfo {
    pub code: String,
    pub is_default: bool,
    pub is_rtl: bool,
}

pub async fn active_locales(pool: &PgPool) -> sqlx::Result<Vec<LocaleInfo>> {
    sqlx::query_as!(
        LocaleInfo,
        "SELECT code, is_default, is_rtl FROM locales WHERE is_active ORDER BY sort_order, code"
    )
    .fetch_all(pool)
    .await
}

/// First active locale among `candidates` (in priority order), else the default locale.
pub fn resolve_locale(candidates: &[Option<String>], active: &[LocaleInfo]) -> String {
    candidates
        .iter()
        .flatten()
        .find(|c| active.iter().any(|l| &l.code == *c))
        .cloned()
        .or_else(|| active.iter().find(|l| l.is_default).map(|l| l.code.clone()))
        .unwrap_or_else(|| "fr".into())
}

/// The active template `code` in `locale`, falling back to the default locale (with a warning)
/// when that translation is missing. `None`: unknown or deactivated template.
pub async fn load(pool: &PgPool, code: &str, locale: &str) -> sqlx::Result<Option<LoadedTemplate>> {
    let row = sqlx::query!(
        r#"SELECT t.code::text AS "code!", t.is_active, t.variables, tr.locale, tr.subject, tr.body_html,
                  tr.body_text, l.is_rtl
           FROM email_templates t
           JOIN email_template_translations tr ON tr.email_template_id = t.id
           JOIN locales l ON l.code = tr.locale
           WHERE t.code = $1::text::citext
             AND (tr.locale = $2 OR l.is_default)
           ORDER BY (tr.locale = $2) DESC
           LIMIT 1"#,
        code,
        locale
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if !row.is_active {
        return Ok(None);
    }
    if row.locale != locale {
        tracing::warn!(template = code, wanted = locale, used = %row.locale, "email template translation missing, using the default locale");
    }
    Ok(Some(LoadedTemplate {
        code: row.code,
        variables: row.variables,
        locale: row.locale,
        rtl: row.is_rtl,
        subject: row.subject,
        body_html: row.body_html,
        body_text: row.body_text,
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("unknown email template {0:?}")]
    UnknownTemplate(String),
    #[error(transparent)]
    Invalid(#[from] RenderError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Creates or replaces a translation, after checking its syntax and that it only uses the
/// template's declared variables.
pub async fn save_translation(
    pool: &PgPool,
    code: &str,
    locale: &str,
    subject: &str,
    body_html: &str,
    body_text: &str,
) -> Result<(), SaveError> {
    let template = sqlx::query!(
        "SELECT id, variables FROM email_templates WHERE code = $1::text::citext",
        code
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| SaveError::UnknownTemplate(code.into()))?;
    render::validate(&TemplateSource {
        subject,
        body_html,
        body_text,
        variables: &template.variables,
    })?;
    sqlx::query!(
        "INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (email_template_id, locale)
         DO UPDATE SET subject = EXCLUDED.subject, body_html = EXCLUDED.body_html, body_text = EXCLUDED.body_text",
        template.id,
        locale,
        subject,
        body_html,
        body_text
    )
    .execute(pool)
    .await?;
    Ok(())
}
