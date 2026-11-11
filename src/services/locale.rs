//! Request locale resolution: `?locale=` first, then `Accept-Language` (by q-value), then the
//! default locale. The candidate list always comes from the `locales` table.

use crate::db::types::Locale;

pub fn resolve_locale(
    query: Option<&str>,
    accept_language: Option<&str>,
    available: &[Locale],
    default: &Locale,
) -> Locale {
    if let Some(found) = query.and_then(|q| match_tag(q, available)) {
        return found;
    }
    if let Some(header) = accept_language {
        let mut ranges: Vec<(f32, &str)> = header
            .split(',')
            .filter_map(|part| {
                let mut pieces = part.trim().split(';');
                let tag = pieces.next()?.trim();
                let q = pieces
                    .find_map(|p| p.trim().strip_prefix("q="))
                    .and_then(|q| q.parse::<f32>().ok())
                    .unwrap_or(1.0);
                (!tag.is_empty() && tag != "*" && q > 0.0).then_some((q, tag))
            })
            .collect();
        ranges.sort_by(|a, b| b.0.total_cmp(&a.0)); // stable: keeps header order on equal q
        if let Some(found) = ranges.iter().find_map(|(_, tag)| match_tag(tag, available)) {
            return found;
        }
    }
    default.clone()
}

/// Exact match (case-insensitive), else the primary subtag (`fr-FR` → `fr`).
fn match_tag(tag: &str, available: &[Locale]) -> Option<Locale> {
    let tag = tag.trim().to_ascii_lowercase();
    let primary = tag.split('-').next().unwrap_or_default();
    available
        .iter()
        .find(|l| l.as_str().eq_ignore_ascii_case(&tag))
        .or_else(|| {
            available
                .iter()
                .find(|l| l.as_str().eq_ignore_ascii_case(primary))
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locales() -> (Vec<Locale>, Locale) {
        (vec!["fr".into(), "en".into(), "ar".into()], "fr".into())
    }

    #[test]
    fn query_parameter_wins() {
        let (all, default) = locales();
        assert_eq!(
            resolve_locale(Some("ar"), Some("en"), &all, &default),
            "ar".into()
        );
        assert_eq!(
            resolve_locale(Some("EN"), None, &all, &default),
            "en".into()
        );
    }

    #[test]
    fn accept_language_is_ordered_by_quality_and_falls_back_to_the_primary_subtag() {
        let (all, default) = locales();
        assert_eq!(
            resolve_locale(None, Some("de-DE, en-GB;q=0.8, ar;q=0.9"), &all, &default),
            "ar".into()
        );
        assert_eq!(
            resolve_locale(None, Some("en-US,en;q=0.9"), &all, &default),
            "en".into()
        );
        assert_eq!(
            resolve_locale(Some("es"), Some("de, it;q=0.5"), &all, &default),
            "fr".into()
        );
        assert_eq!(
            resolve_locale(None, Some("ar;q=0, *"), &all, &default),
            "fr".into()
        );
    }
}
