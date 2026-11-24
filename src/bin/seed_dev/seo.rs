//! Demo SEO data: an activity and a page renamed (automatic 301s through the redirect service),
//! three manual redirects (one used, one never used for months, one external) and a 404 log of
//! ten rows, one of them scanner noise inserted directly (the live filter never records it).

use mouzoun_api::{
    modules::seo::{redirects, routes},
    services::audit::Actor,
};
use sqlx::PgPool;
use uuid::Uuid;

pub async fn seed(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let actor = Actor::system();

    // A renamed activity: /activities/<slug> → /activities/<slug>-au-desert (fr only).
    let activity: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT p.id, t.slug FROM products p JOIN product_translations t ON t.product_id = p.id AND t.locale = 'fr'
         WHERE p.kind = 'activity' AND p.status = 'published' AND p.deleted_at IS NULL ORDER BY p.sort_order, p.id LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, slug)) = activity {
        let before = redirects::snapshot(&mut tx, routes::ACTIVITY, id).await?;
        sqlx::query(
            "UPDATE product_translations SET slug = $2 WHERE product_id = $1 AND locale = 'fr'",
        )
        .bind(id)
        .bind(format!("{slug}-au-desert"))
        .execute(&mut *tx)
        .await?;
        redirects::after_change(&mut tx, &actor, routes::ACTIVITY, id, before).await?;
    }
    // A renamed page (the first published builder page that is not the home page).
    let page: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT p.id, t.slug FROM pages p JOIN page_translations t ON t.page_id = p.id AND t.locale = 'fr'
         WHERE p.page_type <> 'home' AND p.status = 'published' AND p.deleted_at IS NULL ORDER BY p.created_at LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, slug)) = page {
        let before = redirects::snapshot(&mut tx, routes::PAGE, id).await?;
        sqlx::query("UPDATE page_translations SET slug = $2 WHERE page_id = $1 AND locale = 'fr'")
            .bind(id)
            .bind(format!("{slug}-mouzoun"))
            .execute(&mut *tx)
            .await?;
        redirects::after_change(&mut tx, &actor, routes::PAGE, id, before).await?;
    }

    // Manual redirects: used, never used for months, external.
    sqlx::query(
        "INSERT INTO redirects (from_path, to_path, status_code, source, note, hits, last_hit_at, created_at) VALUES
            ('/promo-ete', '/activities', 301, 'manual', 'Ancienne campagne d''été', 42, now() - interval '2 days', now() - interval '1 year'),
            ('/ancienne-carte', '/stays', 302, 'manual', 'Carte imprimée 2024', 0, NULL, now() - interval '8 months'),
            ('/instagram', 'https://www.instagram.com/mouzoun_mssici/', 302, 'manual', 'Lien des cartes de visite', 7, now() - interval '20 days', now() - interval '3 months')
         ON CONFLICT (from_path) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;

    // 404 log: what visitors asked for, and one scanner path the filter would have dropped.
    sqlx::query(
        "INSERT INTO not_found_log (path, hits, first_seen_at, last_seen_at, last_referrer) VALUES
            ('/activites/quad', 31, now() - interval '40 days', now() - interval '1 day', 'https://www.google.com/'),
            ('/boutique/datte-medjool', 18, now() - interval '20 days', now() - interval '2 hours', 'https://www.instagram.com/'),
            ('/sejours', 12, now() - interval '30 days', now() - interval '3 days', NULL),
            ('/en/activity/camel-ride', 9, now() - interval '12 days', now() - interval '1 day', 'https://www.bing.com/'),
            ('/ar/boutique/تمر', 6, now() - interval '9 days', now() - interval '4 days', NULL),
            ('/contactez-nous', 5, now() - interval '50 days', now() - interval '10 days', NULL),
            ('/bivouac-merzouga', 4, now() - interval '15 days', now() - interval '6 days', 'https://www.tripadvisor.fr/'),
            ('/en/stays/luxury-tent', 3, now() - interval '7 days', now() - interval '2 days', NULL),
            ('/boutique/huile', 2, now() - interval '5 days', now() - interval '5 days', NULL),
            ('/wp-login.php', 57, now() - interval '60 days', now() - interval '1 hour', NULL)
         ON CONFLICT (path) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
