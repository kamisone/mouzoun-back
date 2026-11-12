//! Opening online booking: the checklist (`dashboard.read`) and the switch (`settings.write`).
//! Turning booking on is refused while a required check fails; turning it off always works.

use axum::{Json, extract::State, http::StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{
        audit,
        booking::go_live::{self, Checklist},
        outbox::{self, Event},
    },
    state::AppState,
};

pub async fn checklist(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Checklist>> {
    staff.require("dashboard.read")?;
    Ok(Json(
        go_live::run(&state.db, &state.config, Utc::now()).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct SwitchBody {
    pub enabled: bool,
}

pub async fn switch(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<SwitchBody>,
) -> AppResult<Json<Checklist>> {
    staff.require("settings.write")?;
    let now = Utc::now();
    let list = go_live::run(&state.db, &state.config, now).await?;
    if body.enabled && !list.ready {
        let failing: Vec<&str> = list
            .checks
            .iter()
            .filter(|c| c.required && !c.ok)
            .map(|c| c.key)
            .collect();
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "go_live_blocked",
            "booking cannot open before every required check passes",
        )
        .with_details(json!({ "failing": failing })));
    }
    let mut tx = state.db.begin().await?;
    let before = sqlx::query_scalar!(
        "UPDATE settings s SET value = $1 FROM settings old
         WHERE s.key = 'booking.enabled' AND old.key = s.key RETURNING old.value",
        json!(body.enabled)
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "settings.update",
        "setting",
        "booking.enabled",
        json!({ "before": before, "after": body.enabled }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::SettingsUpdated {
            key: "booking.enabled".into(),
        },
    )
    .await?;
    tx.commit().await?;
    // The public funnel reads the cached settings: clear them now, the worker does it again.
    state.cache.delete(["settings:booking"]).await;
    Ok(Json(go_live::run(&state.db, &state.config, now).await?))
}
