use chrono::Utc;
use serde_json::Value;
use sqlx::postgres::PgPool;

use crate::config::Config;
use crate::error::PetroError;
use crate::models::{Credentials, InsertedEvent};
use crate::petroapp_client::{extract_bill_id, extract_vehicle_plate, PetroClient};
use crate::{notify, AppState};

/// Outcome summary of a single sync cycle.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncOutcome {
    pub run_id: i64,
    pub status: String,
    pub bills_fetched: i32,
    pub events_inserted: i32,
    pub unmapped_count: i32,
    pub anomaly_count: i32,
    pub error: Option<String>,
}

/// The conversion statement: raw JSONB bills -> fuel_events. See the architecture
/// plan for the rationale behind every clause.
const CONVERT_SQL: &str = r#"
WITH unprocessed AS (
    SELECT
        b.petroapp_id,
        (b.payload->>'vehicle_id')::bigint                       AS petroapp_vehicle_id,
        (b.payload->>'odo')::int                                 AS odo,
        NULLIF(b.payload->>'number_of_liters','')::numeric       AS liters,
        NULLIF(b.payload->>'cost','')::numeric                   AS cost,
        NULLIF(b.payload->>'delegate_name','')                   AS delegate_name,
        to_char((b.payload->>'date')::timestamp, 'YYYY-MM-DD')   AS bill_date,
        to_char((b.payload->>'date')::timestamp, 'FMHH12:MI AM') AS bill_time
    FROM petroapp_raw_bills b
    WHERE b.processed = false
      AND b.payload->>'vehicle_id' IS NOT NULL
      AND b.payload->>'odo'        IS NOT NULL
      AND b.payload->>'date'       IS NOT NULL
),
mapped AS (
    SELECT u.*, m.db_car_id
    FROM unprocessed u
    JOIN petroapp_vehicle_map m
      ON m.petroapp_vehicle_id = u.petroapp_vehicle_id AND m.active
),
resolved AS (
    SELECT mp.*, c.id AS car_id, c.car_no_plate, c.transporter,
           COALESCE(NULLIF(d.name, ''), mp.delegate_name, '') AS driver_name
    FROM mapped mp
    JOIN cars c        ON c.id = mp.db_car_id AND c.deleted_at IS NULL
    LEFT JOIN drivers d ON d.id = c.driver_id AND d.deleted_at IS NULL
),
derived AS (
    SELECT r.*,
        (SELECT fe.odometer_after
           FROM fuel_events fe
          WHERE fe.car_no_plate = r.car_no_plate
            AND fe.deleted_at IS NULL
            AND fe.date < r.bill_date
          ORDER BY fe.date DESC, fe.odometer_after DESC
          LIMIT 1) AS prev_odo
    FROM resolved r
)
INSERT INTO fuel_events
    (car_id, car_no_plate, driver_name, date, time, liters, price_per_liter, price,
     fuel_rate, transporter, odometer_before, odometer_after, method,
     petroapp_bill_id, created_at, updated_at)
SELECT
    d.car_id, d.car_no_plate, d.driver_name, d.bill_date, d.bill_time,
    COALESCE(d.liters, 0),
    CASE WHEN COALESCE(d.liters, 0) > 0 THEN d.cost / d.liters ELSE 0 END,
    COALESCE(d.cost, 0),
    CASE
        WHEN d.prev_odo IS NULL        THEN 0
        WHEN (d.odo - d.prev_odo) <= 0 THEN 0
        WHEN COALESCE(d.liters, 0) <= 0 THEN 0
        ELSE (d.odo - d.prev_odo)::numeric / d.liters
    END,
    COALESCE(d.transporter, ''),
    COALESCE(d.prev_odo, d.odo),
    d.odo, 'PetroApp', d.petroapp_id, now(), now()
FROM derived d
ON CONFLICT (petroapp_bill_id) WHERE petroapp_bill_id IS NOT NULL DO NOTHING
RETURNING car_no_plate, driver_name, date, "time" AS time, odometer_after,
          liters::float8 AS liters, price::float8 AS price, price_per_liter::float8 AS price_per_liter, 
          fuel_rate::float8 AS fuel_rate, transporter, petroapp_bill_id
"#;

/// Run one full sync cycle. `trigger` is "schedule" or "manual". This function is
/// designed to never panic; all failure paths are recorded in petroapp_sync_runs.
pub async fn run_sync(state: &AppState, trigger: &str) -> SyncOutcome {
    let pool = &state.pool;

    // Open a run row up front so even a crash leaves a "running" trace.
    let run_id: i64 = match sqlx::query_scalar(
        "INSERT INTO petroapp_sync_runs (trigger, status) VALUES ($1, 'running') RETURNING id",
    )
    .bind(trigger)
    .fetch_one(pool)
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "could not open sync run row");
            return SyncOutcome {
                run_id: 0,
                status: "error".into(),
                bills_fetched: 0,
                events_inserted: 0,
                unmapped_count: 0,
                anomaly_count: 0,
                error: Some(e.to_string()),
            };
        }
    };

    match run_inner(state, run_id).await {
        Ok(outcome) => outcome,
        Err(SyncError::Auth) => {
            // Mark credentials expired and alert; this is the loud replacement for
            // the old silent background death.
            let _ = sqlx::query(
                "UPDATE petroapp_credentials SET status = 'expired', updated_at = now() WHERE id = 1",
            )
            .execute(pool)
            .await;
            finish_run(pool, run_id, "auth_failed", 0, 0, 0, 0, Some("token/cookie expired")).await;
            notify::alert(
                &state.cfg,
                &state.http,
                "token/cookie expired — sync paused until you re-login and POST /token.",
            )
            .await;
            SyncOutcome {
                run_id,
                status: "auth_failed".into(),
                bills_fetched: 0,
                events_inserted: 0,
                unmapped_count: 0,
                anomaly_count: 0,
                error: Some("token/cookie expired".into()),
            }
        }
        Err(SyncError::Other(msg)) => {
            finish_run(pool, run_id, "error", 0, 0, 0, 0, Some(&msg)).await;
            notify::alert(&state.cfg, &state.http, &format!("sync run failed: {msg}")).await;
            SyncOutcome {
                run_id,
                status: "error".into(),
                bills_fetched: 0,
                events_inserted: 0,
                unmapped_count: 0,
                anomaly_count: 0,
                error: Some(msg),
            }
        }
    }
}

enum SyncError {
    Auth,
    Other(String),
}
impl From<sqlx::Error> for SyncError {
    fn from(e: sqlx::Error) -> Self {
        SyncError::Other(e.to_string())
    }
}
impl From<PetroError> for SyncError {
    fn from(e: PetroError) -> Self {
        match e {
            PetroError::Auth => SyncError::Auth,
            other => SyncError::Other(other.to_string()),
        }
    }
}

async fn run_inner(state: &AppState, run_id: i64) -> Result<SyncOutcome, SyncError> {
    let pool = &state.pool;
    let cfg = &state.cfg;

    // 1. Load credentials.
    let creds: Option<Credentials> = sqlx::query_as::<_, Credentials>(
        "SELECT token, cookie, status, last_validated_at, updated_at FROM petroapp_credentials WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    let creds = creds.ok_or_else(|| SyncError::Other("no PetroApp credentials configured".into()))?;

    // 2. Determine the fetch window: from (latest stored bill date − 1 day) through
    //    today, so we always re-cover the most recent day to be safe and never
    //    miss a gap. Falls back to the configured lookback when there's no history.
    let last_date: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        r#"SELECT max((payload->>'date')::timestamp)
           FROM petroapp_raw_bills
           WHERE payload->>'date' ~ '^\d{4}-\d{2}-\d{2}'"#,
    )
    .fetch_one(pool)
    .await?;

    let start = match last_date {
        Some(d) => d.date() - chrono::Duration::days(1),
        None => Utc::now().date_naive() - chrono::Duration::days(cfg.sync_lookback_days),
    };

    // Fetch bills (paginated). Auth failures propagate to the caller's handler.
    let bills = state.petro.fetch_bills(&creds, start).await?;
    let pages = bills.pages as i32;
    let fetched = bills.bills.len() as i32;

    // Credentials proven good this cycle.
    sqlx::query(
        "UPDATE petroapp_credentials SET status = 'valid', last_validated_at = now(), updated_at = now() WHERE id = 1",
    )
    .execute(pool)
    .await?;

    // 3. Land raw bills (idempotent).
    land_raw_bills(pool, &bills.bills).await?;

    // 4. Convert unprocessed -> fuel_events, returning the freshly inserted ones.
    let mut inserted: Vec<InsertedEvent> = sqlx::query_as::<_, InsertedEvent>(CONVERT_SQL)
        .fetch_all(pool)
        .await?;
    let bill_ids: Vec<i64> = inserted.iter().map(|e| e.petroapp_bill_id).collect();

    // 5. Mark every resolvable bill processed (inserted now OR already present);
    //    leave the rest unprocessed with an explanatory flag.
    sqlx::query(
        r#"UPDATE petroapp_raw_bills SET processed = true, processed_at = now(), process_error = NULL
           WHERE petroapp_id IN (
               SELECT b.petroapp_id FROM petroapp_raw_bills b
               JOIN petroapp_vehicle_map m
                 ON m.petroapp_vehicle_id = (b.payload->>'vehicle_id')::bigint AND m.active
               JOIN cars c ON c.id = m.db_car_id AND c.deleted_at IS NULL
               WHERE b.processed = false
           )"#,
    )
    .execute(pool)
    .await?;

    let unmapped: i64 = sqlx::query_scalar(
        r#"WITH flagged AS (
               UPDATE petroapp_raw_bills b SET process_error = CASE
                   WHEN EXISTS (
                       SELECT 1 FROM petroapp_vehicle_map m
                       WHERE m.petroapp_vehicle_id = (b.payload->>'vehicle_id')::bigint AND m.active
                   ) THEN 'mapped vehicle has no active car'
                   ELSE 'no active vehicle mapping'
               END
               WHERE b.processed = false
               RETURNING 1
           )
           SELECT count(*) FROM flagged"#,
    )
    .fetch_one(pool)
    .await?;

    // 6. Bump cars.last_fuel_odometer for the inserted bills, guarded by GREATEST
    //    so a more recent manual reading is never rolled back.
    if !bill_ids.is_empty() {
        sqlx::query(
            r#"UPDATE cars c
               SET last_fuel_odometer = GREATEST(c.last_fuel_odometer, sub.max_odo), updated_at = now()
               FROM (
                   SELECT car_no_plate, MAX(odometer_after) AS max_odo
                   FROM fuel_events
                   WHERE petroapp_bill_id = ANY($1)
                   GROUP BY car_no_plate
               ) sub
               WHERE c.car_no_plate = sub.car_no_plate AND c.deleted_at IS NULL"#,
        )
        .bind(&bill_ids)
        .execute(pool)
        .await?;

        // 6b. Self-healing recompute. A late-mapped bill can land chronologically
        //     BETWEEN two existing events, which makes the following event's
        //     odometer_before (and fuel_rate) stale. For every car touched this
        //     run, re-derive odometer_before/fuel_rate for its PetroApp-sourced
        //     events from the immediately preceding reading (manual or PetroApp).
        //     Only PetroApp rows are rewritten, so manually-entered events keep
        //     the values the operator gave them. Ordering matches the baseline
        //     lookup (date, then odometer_after) to avoid AM/PM time-string sorting.
        recompute_chain(pool, &bill_ids).await?;

        // Re-fetch the fully calculated fuel_rate from the database so notifications
        // broadcast the final, correct efficiency rather than the pre-recompute one.
        for ev in &mut inserted {
            if let Ok(Some(rate)) = sqlx::query_scalar::<_, f64>(
                "SELECT fuel_rate::float8 FROM fuel_events WHERE petroapp_bill_id = $1"
            )
            .bind(ev.petroapp_bill_id)
            .fetch_optional(pool)
            .await
            {
                ev.fuel_rate = rate;
            }
        }
    }

    // 7. Notifications.
    let anomalies = notify_results(state, &inserted).await;

    let events_inserted = inserted.len() as i32;
    finish_run(
        pool,
        run_id,
        "success",
        pages,
        fetched,
        events_inserted,
        unmapped as i32,
        None,
    )
    .await;

    if unmapped > 0 {
        notify::alert(
            &state.cfg,
            &state.http,
            &format!("{unmapped} bill(s) could not be mapped to a vehicle — add them via POST /vehicle-map."),
        )
        .await;
    }

    Ok(SyncOutcome {
        run_id,
        status: "success".into(),
        bills_fetched: fetched,
        events_inserted,
        unmapped_count: unmapped as i32,
        anomaly_count: anomalies,
        error: None,
    })
}

/// Re-derive odometer_before and fuel_rate for the PetroApp-sourced events of any
/// car touched this run, using each event's chronologically-preceding reading
/// (manual or PetroApp) as the baseline. Idempotent: a no-op when nothing shifted.
async fn recompute_chain(pool: &PgPool, bill_ids: &[i64]) -> Result<(), SyncError> {
    sqlx::query(
        r#"
        WITH affected AS (
            SELECT DISTINCT car_no_plate
            FROM fuel_events
            WHERE petroapp_bill_id = ANY($1)
        ),
        ordered AS (
            SELECT fe.id, fe.odometer_after, fe.liters, fe.petroapp_bill_id,
                   LAG(fe.odometer_after) OVER (
                       PARTITION BY fe.car_no_plate
                       ORDER BY fe.date, fe.odometer_after
                   ) AS prev_after
            FROM fuel_events fe
            JOIN affected a ON a.car_no_plate = fe.car_no_plate
            WHERE fe.deleted_at IS NULL
        )
        UPDATE fuel_events fe
        SET odometer_before = COALESCE(o.prev_after, fe.odometer_after),
            fuel_rate = CASE
                WHEN o.prev_after IS NULL                  THEN 0
                WHEN (fe.odometer_after - o.prev_after) <= 0 THEN 0
                WHEN fe.liters <= 0                        THEN 0
                ELSE (fe.odometer_after - o.prev_after)::numeric / fe.liters
            END,
            updated_at = now()
        FROM ordered o
        WHERE fe.id = o.id
          AND o.petroapp_bill_id IS NOT NULL
        "#,
    )
    .bind(bill_ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert raw bills idempotently (one row per PetroApp bill id).
async fn land_raw_bills(pool: &PgPool, bills: &[Value]) -> Result<(), SyncError> {
    let mut tx = pool.begin().await?;
    for bill in bills {
        let Some(id) = extract_bill_id(bill) else {
            tracing::warn!("skipping bill without a usable id");
            continue;
        };
        let plate = extract_vehicle_plate(bill);
        sqlx::query(
            r#"INSERT INTO petroapp_raw_bills (petroapp_id, payload, vehicle_plate)
               VALUES ($1, $2, $3)
               ON CONFLICT (petroapp_id) DO NOTHING"#,
        )
        .bind(id)
        .bind(bill)
        .bind(plate)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Send notifications for newly-inserted events and return the anomaly count.
/// A large batch (first backfill) collapses into one summary message.
async fn notify_results(state: &AppState, inserted: &[InsertedEvent]) -> i32 {
    let cfg = &state.cfg;
    let mut anomalies = 0i32;

    for ev in inserted {
        if ev.fuel_rate > 0.0
            && (ev.fuel_rate < cfg.anomaly_rate_min || ev.fuel_rate > cfg.anomaly_rate_max)
        {
            anomalies += 1;
        }
    }

    if inserted.is_empty() {
        return 0;
    }

    if inserted.len() > cfg.notify_per_event_max {
        let msg = format!(
            "Synced {} fuel events ({} flagged as efficiency anomalies).",
            inserted.len(),
            anomalies
        );
        notify::whatsapp(cfg, &state.http, &msg).await;
        return anomalies;
    }

    for ev in inserted {
        let flag = if ev.fuel_rate <= 0.0 {
            " (no baseline yet)"
        } else if ev.fuel_rate < cfg.anomaly_rate_min {
            "\n🚨 *LOW EFFICIENCY ALERT* - Below normal range"
        } else if ev.fuel_rate > cfg.anomaly_rate_max {
            "\n⚠️ *HIGH EFFICIENCY ALERT* - Above normal range"
        } else {
            "\n✅ *Normal fuel efficiency*"
        };
        let msg = format!(
            "🚛 *FUEL EVENT NOTIFICATION*\n\n\
             *Vehicle:* {plate}\n\
             *Driver:* {driver}\n\
             *Date:* {date}\n\
             *Time:* {time}\n\n\
             ⛽ *Fuel Details:*\n\
             - Amount: {liters:.2} L\n\
             - Cost: {price:.2} EGP\n\
             - Price/L: {ppl:.2} EGP\n\
             - Odometer: {odo}\n\
             - Efficiency: {rate:.2} km/L\n\n\
             🏪 *Station:* {station}\n{flag}",
            plate = ev.car_no_plate,
            driver = ev.driver_name,
            date = ev.date,
            time = ev.time,
            liters = ev.liters,
            price = ev.price,
            ppl = ev.price_per_liter,
            odo = ev.odometer_after,
            rate = ev.fuel_rate,
            station = ev.transporter,
            flag = flag,
        );
        notify::whatsapp(cfg, &state.http, &msg).await;
    }

    anomalies
}

#[allow(clippy::too_many_arguments)]
async fn finish_run(
    pool: &PgPool,
    run_id: i64,
    status: &str,
    pages: i32,
    bills: i32,
    inserted: i32,
    unmapped: i32,
    error: Option<&str>,
) {
    let _ = sqlx::query(
        r#"UPDATE petroapp_sync_runs
           SET status = $2, pages_fetched = $3, bills_fetched = $4, events_inserted = $5,
               unmapped_count = $6, error = $7, finished_at = now()
           WHERE id = $1"#,
    )
    .bind(run_id)
    .bind(status)
    .bind(pages)
    .bind(bills)
    .bind(inserted)
    .bind(unmapped)
    .bind(error)
    .execute(pool)
    .await
    .map_err(|e| tracing::error!(error = %e, "failed to finalize sync run"));
    let _ = Utc::now();
}
