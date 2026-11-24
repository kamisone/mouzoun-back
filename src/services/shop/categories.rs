//! Shop categories (`categories.scope = 'shop'`): tree rules, public addresses and their 301
//! redirects, the primary category of a product, manual product order, delete impact.
//!
//! Tree rules are checked here (clear errors before writing) and again by the database
//! (`categories_tree_check`, deferred to commit). See docs/shop/README.md, "Categories".

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppError;

/// Default product sorts a category can have (`categories.default_sort`).
pub const SORTS: &[&str] = &[
    "manual",
    "best_selling",
    "newest",
    "price_asc",
    "price_desc",
    "name",
];
pub const DEFAULT_MAX_DEPTH: i64 = 3;
/// Shop sub-paths a category slug may not take (`/boutique/p/...`, `/boutique/cart`...).
const RESERVED_SLUGS: &[&str] = &["p", "cart", "checkout", "orders"];

pub fn is_reserved(slug: &str) -> bool {
    RESERVED_SLUGS.contains(&slug)
}

/// Lowercase, accents removed, anything else than letters and digits becomes one hyphen. Arabic
/// stays as Unicode letters (as product slugs). Same rule as `utils/slug.ts` on the front.
pub fn slugify(s: &str) -> String {
    crate::modules::shop::admin::catalog::slugify(s)
}

// ---- Addresses ----------------------------------------------------------------------------------

/// Public path of the shop (`slug = None`) or of a category, as the Nuxt routes define them
/// (`pages/shop/[category].vue`): no prefix for the default language, `/en/shop/...`,
/// `/ar/boutique/...`. Fixed like the routes (`shop.route_prefix` only feeds email links).
pub fn shop_path(locale: &str, default_locale: &str, slug: Option<&str>) -> String {
    let prefix = if locale == default_locale {
        String::new()
    } else {
        format!("/{locale}")
    };
    let segment = if locale == "en" { "shop" } else { "boutique" };
    match slug {
        Some(s) => format!("{prefix}/{segment}/{s}"),
        None => format!("{prefix}/{segment}"),
    }
}

pub async fn default_locale(conn: &mut PgConnection) -> sqlx::Result<String> {
    Ok(
        sqlx::query_scalar!("SELECT code FROM locales WHERE is_default")
            .fetch_optional(conn)
            .await?
            .unwrap_or_else(|| "fr".into()),
    )
}

// ---- Tree -----------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeError {
    Cycle(Uuid),
    TooDeep(Uuid),
    UnknownParent(Uuid),
}

impl From<TreeError> for AppError {
    fn from(e: TreeError) -> Self {
        let (code, message, id) = match e {
            TreeError::Cycle(id) => (
                "category_cycle",
                "a category cannot be placed under itself or one of its subcategories",
                id,
            ),
            TreeError::TooDeep(id) => ("category_too_deep", "the tree would be too deep", id),
            TreeError::UnknownParent(id) => {
                ("category_parent_unknown", "unknown parent category", id)
            }
        };
        AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, code, message)
            .with_details(json!({ "id": id }))
    }
}

/// Every node reaches a root without meeting itself, in at most `max_depth` levels.
pub fn check_tree(nodes: &[Node], max_depth: i64) -> Result<(), TreeError> {
    let parent: HashMap<Uuid, Option<Uuid>> = nodes.iter().map(|n| (n.id, n.parent_id)).collect();
    for n in nodes {
        let mut seen = HashSet::from([n.id]);
        let mut depth = 1;
        let mut up = n.parent_id;
        while let Some(p) = up {
            if !seen.insert(p) {
                return Err(TreeError::Cycle(n.id));
            }
            let Some(next) = parent.get(&p) else {
                return Err(TreeError::UnknownParent(n.id));
            };
            depth += 1;
            up = *next;
        }
        if depth > max_depth {
            return Err(TreeError::TooDeep(n.id));
        }
    }
    Ok(())
}

/// Ids of `root` and everything below it.
pub fn subtree(nodes: &[Node], root: Uuid) -> HashSet<Uuid> {
    let mut out = HashSet::from([root]);
    loop {
        let before = out.len();
        for n in nodes {
            if n.parent_id.is_some_and(|p| out.contains(&p)) {
                out.insert(n.id);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

pub async fn nodes(conn: &mut PgConnection) -> sqlx::Result<Vec<Node>> {
    Ok(
        sqlx::query!("SELECT id, parent_id FROM categories WHERE scope = 'shop'")
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(|r| Node {
                id: r.id,
                parent_id: r.parent_id,
            })
            .collect(),
    )
}

pub async fn max_depth(conn: &mut PgConnection) -> sqlx::Result<i64> {
    let v: Option<Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'shop.category_max_depth'")
            .fetch_optional(conn)
            .await?;
    Ok(v.and_then(|v| v.as_i64())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_MAX_DEPTH))
}

/// Fingerprint of the tree shape (ids, parents, order): a tree move sent with an older one is
/// refused (someone else changed the tree meanwhile).
pub async fn tree_version(conn: &mut PgConnection) -> sqlx::Result<String> {
    sqlx::query_scalar!(
        r#"SELECT md5(COALESCE(string_agg(id::text || ':' || COALESCE(parent_id::text, '') || ':' || sort_order, ',' ORDER BY id), '')) AS "v!"
           FROM categories WHERE scope = 'shop'"#
    )
    .fetch_one(conn)
    .await
}

/// The database tree check fires at commit: its messages become the API's error codes.
pub fn map_tree_error(e: sqlx::Error) -> AppError {
    if let Some(db) = e.as_database_error() {
        let msg = db.message();
        for code in ["category_cycle", "category_too_deep", "category_scope"] {
            if msg.contains(code) {
                return AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, code, msg.to_string());
            }
        }
    }
    e.into()
}

// ---- Products in a category ---------------------------------------------------------------------

/// Adds products at the end of the category's manual order (already linked ones keep theirs).
pub async fn link_products(
    conn: &mut PgConnection,
    category: Uuid,
    products: &[Uuid],
) -> sqlx::Result<u64> {
    let mut added = 0;
    for p in products {
        added += sqlx::query!(
            "INSERT INTO product_categories (product_id, category_id, sort_order)
             VALUES ($1, $2, (SELECT COALESCE(max(sort_order) + 1, 0) FROM product_categories WHERE category_id = $2))
             ON CONFLICT DO NOTHING",
            p,
            category
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    ensure_primary(conn, products).await?;
    Ok(added)
}

/// Products that have categories but no primary one get one: the first by category order.
pub async fn ensure_primary(conn: &mut PgConnection, products: &[Uuid]) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE product_categories pc SET is_primary = true
         FROM (SELECT DISTINCT ON (x.product_id) x.product_id, x.category_id
               FROM product_categories x JOIN categories c ON c.id = x.category_id
               WHERE x.product_id = ANY($1)
                 AND NOT EXISTS (SELECT 1 FROM product_categories y WHERE y.product_id = x.product_id AND y.is_primary)
               ORDER BY x.product_id, (c.scope = 'shop') DESC, c.sort_order, c.id) f
         WHERE pc.product_id = f.product_id AND pc.category_id = f.category_id",
        products
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Makes `category` the primary category of `product` (the link must exist).
pub async fn set_primary(
    conn: &mut PgConnection,
    product: Uuid,
    category: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE product_categories SET is_primary = false WHERE product_id = $1 AND is_primary AND category_id <> $2",
        product,
        category
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE product_categories SET is_primary = true WHERE product_id = $1 AND category_id = $2",
        product,
        category
    )
    .execute(conn)
    .await?;
    Ok(())
}

// ---- Delete impact --------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Named {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
}

/// What deleting a category touches. Shipping has no category rules in this shop, so
/// `shipping_rules` is always empty (kept for the dialog and future rules).
#[derive(Debug, Clone, Serialize)]
pub struct Impact {
    pub id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub products: i64,
    /// Products that would have no shop category left if their link is just removed.
    pub products_without_category: i64,
    pub subcategories: Vec<Named>,
    pub promotions: Vec<Named>,
    pub shipping_rules: Vec<Named>,
    pub variations: Vec<Named>,
    pub menu_links: Vec<Named>,
    /// Type the name to confirm (products or subcategories would move).
    pub requires_confirmation: bool,
    /// Active promotions (or shipping rules) target it: they must be changed first.
    pub blocked: bool,
}

pub async fn impact(conn: &mut PgConnection, id: Uuid) -> Result<Impact, AppError> {
    let c = sqlx::query!(
        r#"SELECT c.parent_id, COALESCE(t.name, '') AS "name!"
           FROM categories c LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = 'fr'
           WHERE c.id = $1 AND c.scope = 'shop'"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::NotFound)?;
    let counts = sqlx::query!(
        r#"SELECT count(*) AS "products!",
                  count(*) FILTER (WHERE NOT EXISTS (
                      SELECT 1 FROM product_categories o JOIN categories oc ON oc.id = o.category_id AND oc.scope = 'shop'
                      WHERE o.product_id = pc.product_id AND o.category_id <> $1)) AS "orphans!"
           FROM product_categories pc JOIN products p ON p.id = pc.product_id AND p.deleted_at IS NULL
           WHERE pc.category_id = $1"#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;
    let subcategories = sqlx::query!(
        r#"SELECT c.id, COALESCE(t.name, '') AS "name!" FROM categories c
           LEFT JOIN category_translations t ON t.category_id = c.id AND t.locale = 'fr'
           WHERE c.parent_id = $1 ORDER BY c.sort_order"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| Named {
        id: r.id,
        name: r.name,
        active: None,
    })
    .collect();
    let promotions: Vec<Named> = sqlx::query!(
        r#"SELECT p.id, COALESCE(t.name, p.code::text, p.id::text) AS "name!",
                  (p.is_active AND (p.booking_window IS NULL OR upper(p.booking_window) IS NULL OR upper(p.booking_window) > now())) AS "active!"
           FROM promotion_categories pc JOIN promotions p ON p.id = pc.promotion_id
           LEFT JOIN promotion_translations t ON t.promotion_id = p.id AND t.locale = 'fr'
           WHERE pc.category_id = $1 ORDER BY 3 DESC, 2"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| Named {
        id: r.id,
        name: r.name,
        active: Some(r.active),
    })
    .collect();
    let variations = sqlx::query!(
        r#"SELECT id, COALESCE(admin_label, code::text) AS "admin_label!" FROM variation_attributes WHERE category_id = $1 ORDER BY sort_order"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| Named {
        id: r.id,
        name: r.admin_label,
        active: None,
    })
    .collect();
    let menu_links = sqlx::query!(
        r#"SELECT i.id, m.code::text || ' · ' || COALESCE(t.label, i.link_type) AS "name!"
           FROM nav_menu_items i JOIN nav_menus m ON m.id = i.menu_id
           LEFT JOIN nav_menu_item_translations t ON t.nav_menu_item_id = i.id AND t.locale = 'fr'
           WHERE i.category_id = $1 ORDER BY m.code, i.sort_order"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| Named {
        id: r.id,
        name: r.name,
        active: None,
    })
    .collect();
    let subcategories: Vec<Named> = subcategories;
    let blocked = promotions.iter().any(|p| p.active == Some(true));
    Ok(Impact {
        id,
        requires_confirmation: counts.products > 0 || !subcategories.is_empty(),
        name: c.name,
        parent_id: c.parent_id,
        products: counts.products,
        products_without_category: counts.orphans,
        subcategories,
        promotions,
        shipping_rules: vec![],
        variations,
        menu_links,
        blocked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: u128, parent: Option<u128>) -> Node {
        Node {
            id: Uuid::from_u128(id),
            parent_id: parent.map(Uuid::from_u128),
        }
    }

    #[test]
    fn tree_rules() {
        let ok = [n(1, None), n(2, Some(1)), n(3, Some(2))];
        assert_eq!(check_tree(&ok, 3), Ok(()));
        assert_eq!(
            check_tree(&ok, 2),
            Err(TreeError::TooDeep(Uuid::from_u128(3)))
        );
        let cycle = [n(1, Some(3)), n(2, Some(1)), n(3, Some(2))];
        assert!(matches!(check_tree(&cycle, 9), Err(TreeError::Cycle(_))));
        let own = [n(1, Some(1))];
        assert!(matches!(check_tree(&own, 9), Err(TreeError::Cycle(_))));
        assert_eq!(
            check_tree(&[n(1, Some(9))], 3),
            Err(TreeError::UnknownParent(Uuid::from_u128(1)))
        );
        assert_eq!(
            subtree(&ok, Uuid::from_u128(2)),
            HashSet::from([Uuid::from_u128(2), Uuid::from_u128(3)])
        );
    }

    #[test]
    fn paths_follow_the_routes() {
        assert_eq!(shop_path("fr", "fr", Some("dattes")), "/boutique/dattes");
        assert_eq!(shop_path("en", "fr", Some("dates")), "/en/shop/dates");
        assert_eq!(shop_path("ar", "fr", Some("تمور")), "/ar/boutique/تمور");
        assert_eq!(shop_path("en", "fr", None), "/en/shop");
        assert!(is_reserved("p"));
        assert_eq!(slugify("Huile d'olive & olives"), "huile-d-olive-olives");
    }
}
