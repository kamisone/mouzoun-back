//! Localised reads: slug per locale, field-by-field fallback, alternates.

mod common;

use mouzoun_api::{db::types::Locale, services::catalog};
use sqlx::PgPool;
use uuid::Uuid;

async fn tour(db: &PgPool, status: &str, suffix: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status) VALUES ($1, 'activity', $2)")
        .bind(id)
        .bind(status)
        .execute(db)
        .await
        .unwrap();
    for (locale, name, slug, description) in [
        (
            "fr",
            "Balade à dos de chameau",
            "balade-chameau",
            Some("Au coucher du soleil, dans les dunes."),
        ),
        ("en", "Camel ride", "camel-ride", None),
        ("ar", "", "jawla-jamal", None),
    ] {
        sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug, description, highlights) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(id)
            .bind(locale)
            .bind(name)
            .bind(format!("{slug}{suffix}"))
            .bind(description)
            .bind(if locale == "fr" { vec!["Coucher de soleil".to_string()] } else { vec![] })
            .execute(db)
            .await
            .unwrap();
    }
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn missing_fields_fall_back_to_the_default_locale_one_by_one(db: PgPool) {
    let id = tour(&db, "published", "").await;

    let en = catalog::product_by_slug(&db, &Locale::new("en"), "camel-ride")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(en.id, id);
    assert_eq!(en.locale, Locale::new("en"));
    assert_eq!(en.name, "Camel ride", "own translation kept");
    assert_eq!(en.slug, "camel-ride");
    assert_eq!(
        en.description.as_deref(),
        Some("Au coucher du soleil, dans les dunes."),
        "fallback to fr"
    );
    assert_eq!(
        en.highlights,
        vec!["Coucher de soleil"],
        "empty list falls back too"
    );

    let ar = catalog::product_by_slug(&db, &Locale::new("ar"), "jawla-jamal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ar.name, "Balade à dos de chameau",
        "empty string falls back"
    );
    assert_eq!(
        ar.slug, "jawla-jamal",
        "the slug stays the requested locale's"
    );

    let alternates: Vec<(String, String)> = en
        .alternates
        .iter()
        .map(|a| (a.locale.to_string(), a.slug.clone()))
        .collect();
    assert_eq!(
        alternates,
        vec![
            ("fr".into(), "balade-chameau".into()),
            ("en".into(), "camel-ride".into()),
            ("ar".into(), "jawla-jamal".into())
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_slug_belongs_to_one_locale_and_drafts_are_hidden(db: PgPool) {
    let id = tour(&db, "published", "").await;
    assert!(
        catalog::product_by_slug(&db, &Locale::new("en"), "balade-chameau")
            .await
            .unwrap()
            .is_none(),
        "fr slug, en locale"
    );
    tour(&db, "draft", "-draft").await;
    assert!(
        catalog::product_by_slug(&db, &Locale::new("fr"), "balade-chameau-draft")
            .await
            .unwrap()
            .is_none()
    );

    let full = catalog::load_product(&db, id).await.unwrap().unwrap();
    let mut locales: Vec<String> = full.translations.keys().map(ToString::to_string).collect();
    locales.sort();
    assert_eq!(
        locales,
        vec!["ar", "en", "fr"],
        "translations keyed by locale"
    );
}
