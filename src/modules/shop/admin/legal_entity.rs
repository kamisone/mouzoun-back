//! Admin › Settings › Legal entity: the French seller's identity and the consumer mediator
//! (`GET`/`PUT /v1/admin/settings/legal-entity`, permission `settings.write`). See
//! `services::shop::legal_entity`.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{
        cache_invalidation::{PAGES_PREFIX, SETTINGS_PREFIX},
        shop::{
            legal_entity::{self, LegalEntityForm},
            settings as shop_settings,
        },
    },
    state::AppState,
};

const PERMISSION: &str = "settings.write";

fn body(form: &LegalEntityForm) -> Value {
    json!({ "entity": form, "missing": form.missing(), "mediator_missing": form.mediator_missing() })
}

pub async fn get(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require(PERMISSION)?;
    let mut conn = state.db.acquire().await?;
    let (_, form) = legal_entity::load(&mut conn).await?;
    Ok(Json(body(&form)))
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Json(input): Json<LegalEntityForm>,
) -> AppResult<Json<Value>> {
    staff.require(PERMISSION)?;
    let form = input.normalized();
    let errors = form.errors();
    if !errors.is_empty() {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_fields",
            "some fields are not valid",
        )
        .with_details(json!({ "fields": errors })));
    }
    let mut tx = state.db.begin().await?;
    let changed = legal_entity::save(&mut tx, &staff.actor(), &form).await?;
    tx.commit().await?;
    if !changed.is_empty() {
        // The legal pages render these fields (tokens): drop their cached renderings, and the
        // cached settings (mediator).
        let settings = shop_settings::load(&state.db).await?;
        for page in settings.legal_pages.values() {
            let _ = state
                .cache
                .delete_prefix(&format!("{PAGES_PREFIX}{page}:"))
                .await;
        }
        let _ = state.cache.delete_prefix(SETTINGS_PREFIX).await;
    }
    let mut out = body(&form);
    out["changed"] = json!(changed);
    Ok(Json(out))
}
