//! Stock: levels per (variant, location), an append-only ledger, reservations and lots.
//!
//! Rules (tested against Postgres in `tests/it/shop_inventory.rs`):
//! - `on_hand` changes only through [`apply_movement`], which writes exactly one
//!   `inventory_movements` row in the same transaction (the sum of the ledger is `on_hand`);
//! - `reserved` holds stock for unpaid orders; the CHECK `reserved <= on_hand + backorder_limit`
//!   is the final guard against overselling (a violation becomes `out_of_stock`);
//! - levels are always locked in the same order (variant_id, location_id) to avoid deadlocks;
//! - lot-tracked variants: lot quantities sum to `on_hand`; stock leaves FEFO (earliest
//!   best-before first, undated lots last).

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::events::{self, ShopEvent};
use crate::db::shop::MovementReason;

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    /// Not enough sellable stock (the CHECK refused, or the pre-check did).
    #[error("out of stock")]
    OutOfStock { variant_id: Uuid },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<InventoryError> for crate::error::AppError {
    fn from(e: InventoryError) -> Self {
        match e {
            InventoryError::OutOfStock { variant_id } => crate::error::AppError::domain(
                axum::http::StatusCode::CONFLICT,
                "out_of_stock",
                "not enough stock",
            )
            .with_details(serde_json::json!({ "variant_id": variant_id })),
            InventoryError::Invalid(m) => crate::error::AppError::BadRequest(m),
            InventoryError::Database(e) => e.into(),
        }
    }
}

fn is_check_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23514")
}

/// References written on a movement.
#[derive(Debug, Clone, Default)]
pub struct MovementRefs {
    pub order_id: Option<Uuid>,
    pub fulfillment_id: Option<Uuid>,
    pub return_id: Option<Uuid>,
    pub transfer_id: Option<Uuid>,
    pub unit_cost_minor: Option<i64>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    InStock,
    LowStock,
    OutOfStock,
    Backorder,
}

/// Availability of `available` units (`on_hand - reserved`, plus backorder when allowed).
pub fn availability(on_hand_minus_reserved: i64, backorder: i64, threshold: i64) -> Availability {
    if on_hand_minus_reserved <= 0 {
        if on_hand_minus_reserved + backorder > 0 {
            Availability::Backorder
        } else {
            Availability::OutOfStock
        }
    } else if on_hand_minus_reserved <= threshold {
        Availability::LowStock
    } else {
        Availability::InStock
    }
}

/// Creates the level row when missing (zero stock).
pub async fn ensure_level(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO inventory_levels (variant_id, location_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        variant_id,
        location_id
    )
    .execute(conn)
    .await?;
    Ok(())
}

struct LevelState {
    on_hand: i64,
    reserved: i64,
    backorder: i64,
}

async fn level_state(
    conn: &mut PgConnection,
    variant_id: Uuid,
) -> sqlx::Result<(LevelState, i64, Uuid)> {
    // Online sellable stock of the variant (locations that fulfil online orders), its threshold.
    let row = sqlx::query!(
        r#"SELECT COALESCE(sum(l.on_hand), 0)::bigint AS "on_hand!", COALESCE(sum(l.reserved), 0)::bigint AS "reserved!",
                  COALESCE(sum(l.backorder_limit), 0)::bigint AS "backorder!",
                  v.low_stock_threshold, v.product_id
           FROM product_variants v
           LEFT JOIN inventory_levels l ON l.variant_id = v.id
                AND l.location_id IN (SELECT id FROM stock_locations s WHERE is_active AND (fulfills_online_orders OR is_pickup_point)
                                       AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id()))
           WHERE v.id = $1
           GROUP BY v.id"#,
        variant_id
    )
    .fetch_one(&mut *conn)
    .await?;
    let threshold = match row.low_stock_threshold {
        Some(t) => i64::from(t),
        None => {
            sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'shop.low_stock_threshold'")
                .fetch_optional(&mut *conn)
                .await?
                .and_then(|v| v.as_i64())
                .unwrap_or(5)
        }
    };
    Ok((
        LevelState {
            on_hand: row.on_hand,
            reserved: row.reserved,
            backorder: row.backorder,
        },
        threshold,
        row.product_id,
    ))
}

/// Emits `stock.low`, `stock.back_in_stock` and `stock.changed` when the availability status of
/// the variant changed between `before` and now.
async fn after_change(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    before: Availability,
) -> sqlx::Result<()> {
    let (now, threshold, product_id) = level_state(conn, variant_id).await?;
    let after = availability(now.on_hand - now.reserved, now.backorder, threshold);
    if after == before {
        return Ok(());
    }
    events::enqueue(conn, ShopEvent::StockChanged { product_id }).await?;
    if matches!(after, Availability::LowStock | Availability::OutOfStock)
        && !matches!(before, Availability::LowStock | Availability::OutOfStock)
    {
        events::enqueue(
            conn,
            ShopEvent::StockLow {
                variant_id,
                location_id,
            },
        )
        .await?;
    }
    if before == Availability::OutOfStock
        && matches!(after, Availability::InStock | Availability::LowStock)
    {
        events::enqueue(conn, ShopEvent::StockBackInStock { variant_id }).await?;
    }
    Ok(())
}

async fn status_now(conn: &mut PgConnection, variant_id: Uuid) -> sqlx::Result<Availability> {
    let (s, threshold, _) = level_state(conn, variant_id).await?;
    Ok(availability(s.on_hand - s.reserved, s.backorder, threshold))
}

/// The only way `on_hand` changes: updates the level (and the lot), writes one movement, and
/// emits the stock events. Returns `on_hand` after the change.
pub async fn apply_movement(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    lot_id: Option<Uuid>,
    delta: i32,
    reason: MovementReason,
    refs: &MovementRefs,
) -> Result<i32, InventoryError> {
    if delta == 0 {
        return Err(InventoryError::Invalid("a movement cannot be zero".into()));
    }
    ensure_level(conn, variant_id, location_id).await?;
    let before = status_now(conn, variant_id).await?;
    let on_hand_after = match sqlx::query_scalar!(
        "UPDATE inventory_levels SET on_hand = on_hand + $3
         WHERE variant_id = $1 AND location_id = $2 RETURNING on_hand",
        variant_id,
        location_id,
        delta
    )
    .fetch_one(&mut *conn)
    .await
    {
        Ok(v) => v,
        Err(e) if is_check_violation(&e) => return Err(InventoryError::OutOfStock { variant_id }),
        Err(e) => return Err(e.into()),
    };
    if let Some(lot) = lot_id {
        let updated = sqlx::query!(
            "UPDATE inventory_lots SET quantity = quantity + $2
             WHERE id = $1 AND variant_id = $3 AND location_id = $4",
            lot,
            delta,
            variant_id,
            location_id
        )
        .execute(&mut *conn)
        .await;
        match updated {
            Ok(r) if r.rows_affected() == 1 => {}
            Ok(_) => return Err(InventoryError::Invalid("unknown lot for this stock".into())),
            Err(e) if is_check_violation(&e) => {
                return Err(InventoryError::Invalid(
                    "the lot does not hold that many units".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    sqlx::query!(
        "INSERT INTO inventory_movements (variant_id, location_id, lot_id, delta, on_hand_after, reason, order_id,
                                          fulfillment_id, return_id, transfer_id, unit_cost_minor, note, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        variant_id,
        location_id,
        lot_id,
        delta,
        on_hand_after,
        reason.as_str(),
        refs.order_id,
        refs.fulfillment_id,
        refs.return_id,
        refs.transfer_id,
        refs.unit_cost_minor,
        refs.note.as_deref(),
        refs.created_by
    )
    .execute(&mut *conn)
    .await?;
    after_change(conn, variant_id, location_id, before).await?;
    Ok(on_hand_after)
}

async fn tracks_lots(conn: &mut PgConnection, variant_id: Uuid) -> sqlx::Result<bool> {
    Ok(sqlx::query_scalar!(
        "SELECT sp.track_lots FROM product_variants v JOIN shop_products sp ON sp.product_id = v.product_id WHERE v.id = $1",
        variant_id
    )
    .fetch_optional(conn)
    .await?
    .unwrap_or(false))
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewLot {
    pub lot_code: String,
    pub produced_on: Option<NaiveDate>,
    pub harvested_on: Option<NaiveDate>,
    pub best_before: Option<NaiveDate>,
    pub notes: Option<String>,
}

/// Goods received (harvest, purchase). Lot-tracked variants need a lot (created if new).
pub async fn receive(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    quantity: i32,
    lot: Option<&NewLot>,
    refs: &MovementRefs,
) -> Result<i32, InventoryError> {
    if quantity <= 0 {
        return Err(InventoryError::Invalid("quantity must be positive".into()));
    }
    ensure_level(conn, variant_id, location_id).await?;
    let lot_id = match (lot, tracks_lots(conn, variant_id).await?) {
        (Some(l), _) => Some(upsert_lot(conn, variant_id, location_id, l).await?),
        (None, true) => {
            return Err(InventoryError::Invalid(
                "this product tracks lots: give the lot".into(),
            ));
        }
        (None, false) => None,
    };
    apply_movement(
        conn,
        variant_id,
        location_id,
        lot_id,
        quantity,
        MovementReason::Receipt,
        refs,
    )
    .await
}

async fn upsert_lot(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    l: &NewLot,
) -> sqlx::Result<Uuid> {
    let code = l.lot_code.trim();
    sqlx::query_scalar!(
        "INSERT INTO inventory_lots (id, variant_id, location_id, lot_code, produced_on, harvested_on, best_before, notes)
         VALUES ($1, $2, $3, $4::text::citext, $5, $6, $7, $8)
         ON CONFLICT (variant_id, location_id, lot_code) DO UPDATE
             SET best_before = COALESCE(EXCLUDED.best_before, inventory_lots.best_before),
                 produced_on = COALESCE(EXCLUDED.produced_on, inventory_lots.produced_on),
                 harvested_on = COALESCE(EXCLUDED.harvested_on, inventory_lots.harvested_on)
         RETURNING id",
        Uuid::now_v7(),
        variant_id,
        location_id,
        code,
        l.produced_on,
        l.harvested_on,
        l.best_before,
        l.notes.as_deref()
    )
    .fetch_one(conn)
    .await
}

/// Lots of a level in FEFO order: earliest best-before first, undated last.
async fn fefo_lots(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
) -> sqlx::Result<Vec<(Uuid, i32)>> {
    Ok(sqlx::query!(
        "SELECT id, quantity FROM inventory_lots
         WHERE variant_id = $1 AND location_id = $2 AND quantity > 0
         ORDER BY best_before ASC NULLS LAST, created_at, id
         FOR UPDATE",
        variant_id,
        location_id
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| (r.id, r.quantity))
    .collect())
}

/// Takes `quantity` out of stock: FEFO across lots for lot-tracked variants (one movement per
/// lot touched), else one movement. Returns `(lot_id, quantity)` taken.
pub async fn take_out(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    quantity: i32,
    reason: MovementReason,
    refs: &MovementRefs,
) -> Result<Vec<(Option<Uuid>, i32)>, InventoryError> {
    if !tracks_lots(conn, variant_id).await? {
        apply_movement(conn, variant_id, location_id, None, -quantity, reason, refs).await?;
        return Ok(vec![(None, quantity)]);
    }
    let mut left = quantity;
    let mut taken = Vec::new();
    for (lot, available) in fefo_lots(conn, variant_id, location_id).await? {
        if left == 0 {
            break;
        }
        let q = left.min(available);
        apply_movement(conn, variant_id, location_id, Some(lot), -q, reason, refs).await?;
        taken.push((Some(lot), q));
        left -= q;
    }
    if left > 0 {
        return Err(InventoryError::OutOfStock { variant_id });
    }
    Ok(taken)
}

/// Sets the counted quantity (stocktake): writes the difference as a `count` movement.
pub async fn count(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    lot_id: Option<Uuid>,
    counted: i32,
    refs: &MovementRefs,
) -> Result<i32, InventoryError> {
    if counted < 0 {
        return Err(InventoryError::Invalid("a count cannot be negative".into()));
    }
    ensure_level(conn, variant_id, location_id).await?;
    let current = match lot_id {
        Some(lot) => sqlx::query_scalar!(
            "SELECT quantity FROM inventory_lots WHERE id = $1 AND variant_id = $2 AND location_id = $3 FOR UPDATE",
            lot,
            variant_id,
            location_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| InventoryError::Invalid("unknown lot".into()))?,
        None => {
            if tracks_lots(conn, variant_id).await? {
                return Err(InventoryError::Invalid("this product tracks lots: count each lot".into()));
            }
            sqlx::query_scalar!(
                "SELECT on_hand FROM inventory_levels WHERE variant_id = $1 AND location_id = $2 FOR UPDATE",
                variant_id,
                location_id
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    let delta = counted - current;
    if delta == 0 {
        return Ok(current);
    }
    apply_movement(
        conn,
        variant_id,
        location_id,
        lot_id,
        delta,
        MovementReason::Count,
        refs,
    )
    .await
}

/// Moves stock between two locations: two movements sharing one `transfer_id` (the total is
/// unchanged). Lots keep their code and dates at the destination.
pub async fn transfer(
    conn: &mut PgConnection,
    variant_id: Uuid,
    from: Uuid,
    to: Uuid,
    quantity: i32,
    refs: &MovementRefs,
) -> Result<Uuid, InventoryError> {
    if from == to || quantity <= 0 {
        return Err(InventoryError::Invalid(
            "choose two locations and a positive quantity".into(),
        ));
    }
    // Stable lock order.
    let (a, b) = if from < to { (from, to) } else { (to, from) };
    for loc in [a, b] {
        ensure_level(conn, variant_id, loc).await?;
        sqlx::query!(
            "SELECT on_hand FROM inventory_levels WHERE variant_id = $1 AND location_id = $2 FOR UPDATE",
            variant_id,
            loc
        )
        .fetch_one(&mut *conn)
        .await?;
    }
    let transfer_id = Uuid::now_v7();
    let refs = MovementRefs {
        transfer_id: Some(transfer_id),
        ..refs.clone()
    };
    let taken = take_out(
        conn,
        variant_id,
        from,
        quantity,
        MovementReason::TransferOut,
        &refs,
    )
    .await?;
    for (lot, q) in taken {
        let dest_lot = match lot {
            Some(lot) => {
                let l = sqlx::query!(
                    "SELECT lot_code::text AS \"code!\", produced_on, harvested_on, best_before, notes FROM inventory_lots WHERE id = $1",
                    lot
                )
                .fetch_one(&mut *conn)
                .await?;
                Some(
                    upsert_lot(
                        conn,
                        variant_id,
                        to,
                        &NewLot {
                            lot_code: l.code,
                            produced_on: l.produced_on,
                            harvested_on: l.harvested_on,
                            best_before: l.best_before,
                            notes: l.notes,
                        },
                    )
                    .await?,
                )
            }
            None => None,
        };
        apply_movement(
            conn,
            variant_id,
            to,
            dest_lot,
            q,
            MovementReason::TransferIn,
            &refs,
        )
        .await?;
    }
    Ok(transfer_id)
}

// ---- Reservations -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReserveLine {
    pub order_item_id: Uuid,
    pub variant_id: Uuid,
    pub quantity: i32,
}

/// Holds stock for an order. Location: `pickup_location` when given, else the first active
/// location fulfilling online orders that can serve every line, else each line from wherever
/// it is available (split). Untracked variants are skipped. `expires_at` `None` = until
/// staff confirm or cancel (cash on delivery).
pub async fn reserve(
    conn: &mut PgConnection,
    order_id: Uuid,
    lines: &[ReserveLine],
    pickup_location: Option<Uuid>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), InventoryError> {
    // Tracked lines only, merged per variant.
    let tracked: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT id FROM product_variants WHERE id = ANY($1) AND track_inventory",
        &lines.iter().map(|l| l.variant_id).collect::<Vec<_>>()
    )
    .fetch_all(&mut *conn)
    .await?;
    let lines: Vec<&ReserveLine> = lines
        .iter()
        .filter(|l| tracked.contains(&l.variant_id))
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    let mut needed: BTreeMap<Uuid, i64> = BTreeMap::new();
    for l in &lines {
        *needed.entry(l.variant_id).or_default() += i64::from(l.quantity);
    }
    let variants: Vec<Uuid> = needed.keys().copied().collect();

    let locations: Vec<Uuid> = match pickup_location {
        Some(id) => vec![id],
        None => sqlx::query_scalar!(
            "SELECT id FROM stock_locations s WHERE is_active AND fulfills_online_orders AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id()) ORDER BY sort_order, id"
        )
        .fetch_all(&mut *conn)
        .await?,
    };
    if locations.is_empty() {
        return Err(InventoryError::OutOfStock {
            variant_id: variants[0],
        });
    }
    for v in &variants {
        for loc in &locations {
            ensure_level(conn, *v, *loc).await?;
        }
    }
    // Lock every candidate level in a stable order (variant, location).
    let levels = sqlx::query!(
        r#"SELECT variant_id, location_id, (on_hand + backorder_limit - reserved)::bigint AS "free!"
           FROM inventory_levels WHERE variant_id = ANY($1) AND location_id = ANY($2)
           ORDER BY variant_id, location_id FOR UPDATE"#,
        &variants,
        &locations
    )
    .fetch_all(&mut *conn)
    .await?;
    let free = |v: Uuid, loc: Uuid| -> i64 {
        levels
            .iter()
            .find(|l| l.variant_id == v && l.location_id == loc)
            .map_or(0, |l| l.free)
    };
    let befores: Vec<(Uuid, Availability)> = {
        let mut out = Vec::new();
        for v in &variants {
            out.push((*v, status_now(conn, *v).await?));
        }
        out
    };

    // One location serving everything, in location order.
    let single = locations
        .iter()
        .find(|loc| needed.iter().all(|(v, q)| free(*v, **loc) >= *q))
        .copied();
    // (order_item_id, variant, location, quantity)
    let mut plan: Vec<(Uuid, Uuid, Uuid, i32)> = Vec::new();
    match single {
        Some(loc) => {
            for l in &lines {
                plan.push((l.order_item_id, l.variant_id, loc, l.quantity));
            }
        }
        None => {
            let mut remaining: BTreeMap<(Uuid, Uuid), i64> = BTreeMap::new();
            for v in &variants {
                for loc in &locations {
                    remaining.insert((*v, *loc), free(*v, *loc).max(0));
                }
            }
            for l in &lines {
                let mut left = i64::from(l.quantity);
                for loc in &locations {
                    if left == 0 {
                        break;
                    }
                    let r = remaining.get_mut(&(l.variant_id, *loc)).expect("seeded");
                    let take = left.min(*r);
                    if take > 0 {
                        plan.push((l.order_item_id, l.variant_id, *loc, take as i32));
                        *r -= take;
                        left -= take;
                    }
                }
                if left > 0 {
                    return Err(InventoryError::OutOfStock {
                        variant_id: l.variant_id,
                    });
                }
            }
        }
    }
    // Apply in lock order.
    plan.sort_by_key(|(_, v, loc, _)| (*v, *loc));
    for (item, v, loc, q) in plan {
        let res = sqlx::query!(
            "UPDATE inventory_levels SET reserved = reserved + $3 WHERE variant_id = $1 AND location_id = $2",
            v,
            loc,
            q
        )
        .execute(&mut *conn)
        .await;
        match res {
            Ok(_) => {}
            Err(e) if is_check_violation(&e) => {
                return Err(InventoryError::OutOfStock { variant_id: v });
            }
            Err(e) => return Err(e.into()),
        }
        sqlx::query!(
            "INSERT INTO inventory_reservations (id, order_id, order_item_id, variant_id, location_id, quantity, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            Uuid::now_v7(),
            order_id,
            item,
            v,
            loc,
            q,
            expires_at
        )
        .execute(&mut *conn)
        .await?;
    }
    for (v, before) in befores {
        let loc = locations[0];
        after_change(conn, v, loc, before).await?;
    }
    Ok(())
}

/// Releases the open reservations of an order (unpaid, cancelled, expired). Optionally only for
/// some order items (order edits). Returns the number of reservations released.
pub async fn release(
    conn: &mut PgConnection,
    order_id: Uuid,
    only_items: Option<&[Uuid]>,
    now: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let rows = sqlx::query!(
        "SELECT id, variant_id, location_id, quantity FROM inventory_reservations
         WHERE order_id = $1 AND released_at IS NULL AND consumed_at IS NULL
           AND ($2::uuid[] IS NULL OR order_item_id = ANY($2))
         ORDER BY variant_id, location_id FOR UPDATE",
        order_id,
        only_items.map(<[Uuid]>::to_vec) as Option<Vec<Uuid>>
    )
    .fetch_all(&mut *conn)
    .await?;
    for r in &rows {
        let before = status_now(conn, r.variant_id).await?;
        sqlx::query!(
            "UPDATE inventory_levels SET reserved = reserved - $3 WHERE variant_id = $1 AND location_id = $2",
            r.variant_id,
            r.location_id,
            r.quantity
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "UPDATE inventory_reservations SET released_at = $2 WHERE id = $1",
            r.id,
            now
        )
        .execute(&mut *conn)
        .await?;
        after_change(conn, r.variant_id, r.location_id, before).await?;
    }
    Ok(rows.len() as u64)
}

/// The stock leaves for good (order paid, or handed over for offline payments): for each open
/// reservation `on_hand -= q` and `reserved -= q` with one `sale` movement (per lot, FEFO).
/// Untracked lines have no reservation and no movement. `only_items` limits it to some lines.
pub async fn consume(
    conn: &mut PgConnection,
    order_id: Uuid,
    only_items: Option<&[Uuid]>,
    now: DateTime<Utc>,
) -> Result<u64, InventoryError> {
    let rows = sqlx::query!(
        "SELECT id, variant_id, location_id, quantity FROM inventory_reservations
         WHERE order_id = $1 AND released_at IS NULL AND consumed_at IS NULL
           AND ($2::uuid[] IS NULL OR order_item_id = ANY($2))
         ORDER BY variant_id, location_id FOR UPDATE",
        order_id,
        only_items.map(<[Uuid]>::to_vec) as Option<Vec<Uuid>>
    )
    .fetch_all(&mut *conn)
    .await?;
    for r in &rows {
        // Unreserve first (the sale movement then lowers on_hand within the CHECK).
        sqlx::query!(
            "UPDATE inventory_levels SET reserved = reserved - $3 WHERE variant_id = $1 AND location_id = $2",
            r.variant_id,
            r.location_id,
            r.quantity
        )
        .execute(&mut *conn)
        .await?;
        take_out(
            conn,
            r.variant_id,
            r.location_id,
            r.quantity,
            MovementReason::Sale,
            &MovementRefs {
                order_id: Some(order_id),
                ..MovementRefs::default()
            },
        )
        .await?;
        sqlx::query!(
            "UPDATE inventory_reservations SET consumed_at = $2 WHERE id = $1",
            r.id,
            now
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(rows.len() as u64)
}

/// Returned goods back in stock (sellable condition): one `return_restock` movement, into the
/// lot they left from when known.
pub async fn restock_return(
    conn: &mut PgConnection,
    variant_id: Uuid,
    location_id: Uuid,
    lot_id: Option<Uuid>,
    quantity: i32,
    refs: &MovementRefs,
) -> Result<i32, InventoryError> {
    let lot_id = match lot_id {
        Some(l) => Some(l),
        None if tracks_lots(conn, variant_id).await? => {
            // Unknown lot: a dedicated "returns" lot (no best-before: staff should check it).
            Some(
                upsert_lot(
                    conn,
                    variant_id,
                    location_id,
                    &NewLot {
                        lot_code: "RETURNS".into(),
                        produced_on: None,
                        harvested_on: None,
                        best_before: None,
                        notes: Some("returned goods".into()),
                    },
                )
                .await?,
            )
        }
        None => None,
    };
    apply_movement(
        conn,
        variant_id,
        location_id,
        lot_id,
        quantity,
        MovementReason::ReturnRestock,
        refs,
    )
    .await
}

/// Background job: releases reservations past their expiry and returns the orders touched.
pub async fn release_expired(pool: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Vec<Uuid>> {
    let mut tx = pool.begin().await?;
    let orders: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT DISTINCT order_id AS \"order_id!\" FROM (
             SELECT order_id FROM inventory_reservations
             WHERE expires_at <= $1 AND released_at IS NULL AND consumed_at IS NULL
             ORDER BY expires_at LIMIT 200 FOR UPDATE SKIP LOCKED) r",
        now
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(orders)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_statuses() {
        assert_eq!(availability(10, 0, 5), Availability::InStock);
        assert_eq!(availability(5, 0, 5), Availability::LowStock);
        assert_eq!(availability(0, 0, 5), Availability::OutOfStock);
        assert_eq!(availability(0, 3, 5), Availability::Backorder);
        assert_eq!(availability(-2, 3, 5), Availability::Backorder);
    }
}
