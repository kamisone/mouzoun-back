//! Signed links of the shop: a guest opens their order page from an email without an account
//! (`?t=` token bound to the order number), and pickup codes are derived from the same secret.

/// Key derived from the app secret (`JWT_SECRET`) for one purpose.
fn derived(secret: &str, purpose: &str) -> ring::hmac::Key {
    let root = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let k = ring::hmac::sign(&root, purpose.as_bytes());
    ring::hmac::Key::new(ring::hmac::HMAC_SHA256, k.as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Access token of an order page (no expiry: it only shows the order, like the email does).
pub fn order_token(secret: &str, number: &str) -> String {
    let tag = ring::hmac::sign(&derived(secret, "shop-order-link"), number.as_bytes());
    hex(&tag.as_ref()[..16])
}

pub fn order_token_valid(secret: &str, number: &str, token: &str) -> bool {
    crate::services::accounts::tokens::constant_time_eq(
        order_token(secret, number).as_bytes(),
        token.trim().as_bytes(),
    )
}

/// Public shop URL: `{site}[/{locale}]/{prefix}/{path}`. The default locale has no prefix
/// (Nuxt i18n `prefix_except_default`); `prefix` comes from the `shop.route_prefix` setting.
pub fn shop_url(
    site_url: &str,
    locale: &str,
    default_locale: &str,
    prefixes: &serde_json::Value,
    path: &str,
) -> String {
    let prefix = crate::services::shop::settings::ShopSettings::localized(prefixes, locale);
    let prefix = if prefix.is_empty() {
        "shop".to_string()
    } else {
        prefix
    };
    let lang = if locale == default_locale {
        String::new()
    } else {
        format!("/{locale}")
    };
    let path = path.trim_start_matches('/');
    let tail = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    format!("{}{lang}/{prefix}{tail}", site_url.trim_end_matches('/'))
}

/// The order page, signed so a guest opens it straight from the email.
pub fn order_url(
    site_url: &str,
    locale: &str,
    default_locale: &str,
    prefixes: &serde_json::Value,
    number: &str,
    secret: &str,
) -> String {
    format!(
        "{}?t={}",
        shop_url(
            site_url,
            locale,
            default_locale,
            prefixes,
            &format!("orders/{number}")
        ),
        order_token(secret, number)
    )
}

/// Secret the 6-digit pickup codes are derived from.
pub fn pickup_secret(secret: &str) -> Vec<u8> {
    ring::hmac::sign(&derived(secret, "shop-pickup-code"), b"v1")
        .as_ref()
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_tokens_are_bound_to_the_number() {
        let t = order_token("s", "MZS-27-000001");
        assert!(order_token_valid("s", "MZS-27-000001", &t));
        assert!(!order_token_valid("s", "MZS-27-000002", &t));
        assert!(!order_token_valid("other", "MZS-27-000001", &t));
        assert_eq!(t.len(), 32);
    }

    #[test]
    fn shop_urls_follow_the_locale_strategy() {
        let prefixes = serde_json::json!({ "fr": "boutique", "en": "shop", "ar": "boutique" });
        assert_eq!(
            shop_url("https://m.ma/", "fr", "fr", &prefixes, "cart"),
            "https://m.ma/boutique/cart"
        );
        assert_eq!(
            shop_url("https://m.ma", "en", "fr", &prefixes, "p/dates"),
            "https://m.ma/en/shop/p/dates"
        );
        assert_eq!(
            shop_url("https://m.ma", "ar", "fr", &prefixes, ""),
            "https://m.ma/ar/boutique"
        );
    }
}
