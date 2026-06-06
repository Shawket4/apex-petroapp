use actix_web::{web, HttpRequest, HttpResponse};
use serde_json::json;

use crate::error::ApiError;
use crate::models::{
    Credentials, OdometerPushBody, SetCredentialsBody, SyncRun, UpsertVehicleBody, VehicleMapRow,
};
use crate::AppState;

/// Guard for mutating endpoints: requires X-Api-Key when INTERNAL_API_KEY is set.
fn check_key(req: &HttpRequest, st: &AppState) -> Result<(), ApiError> {
    match &st.cfg.internal_api_key {
        None => Ok(()),
        Some(expected) => {
            let got = req
                .headers()
                .get("X-Api-Key")
                .and_then(|v| v.to_str().ok());
            if got == Some(expected.as_str()) {
                Ok(())
            } else {
                Err(ApiError::Unauthorized)
            }
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/health", web::get().to(health))
        .route("/status", web::get().to(status))
        .route("/token", web::post().to(set_token))
        .route("/sync/trigger", web::post().to(sync_trigger))
        .route("/sync/runs", web::get().to(sync_runs))
        .route("/vehicle-map", web::get().to(list_vehicle_map))
        .route("/vehicle-map", web::post().to(upsert_vehicle_map))
        .route("/vehicle-map/{petroapp_vehicle_id}", web::delete().to(deactivate_vehicle_map))
        .route("/odometer/push", web::post().to(odometer_push));
}

async fn health(st: web::Data<AppState>) -> Result<HttpResponse, ApiError> {
    sqlx::query("SELECT 1").execute(&st.pool).await?;
    Ok(HttpResponse::Ok().json(json!({ "status": "ok" })))
}

async fn status(st: web::Data<AppState>) -> Result<HttpResponse, ApiError> {
    let creds: Option<Credentials> = sqlx::query_as::<_, Credentials>(
        "SELECT token, cookie, status, last_validated_at, updated_at FROM petroapp_credentials WHERE id = 1",
    )
    .fetch_optional(&st.pool)
    .await?;

    let last_run: Option<SyncRun> = sqlx::query_as::<_, SyncRun>(
        "SELECT id, trigger, status, pages_fetched, bills_fetched, events_inserted, \
         unmapped_count, anomaly_count, error, started_at, finished_at \
         FROM petroapp_sync_runs ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_optional(&st.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({
        "credentials_status": creds.as_ref().map(|c| c.status.clone()).unwrap_or_else(|| "missing".into()),
        "credentials_validated_at": creds.and_then(|c| c.last_validated_at),
        "last_run": last_run,
    })))
}

async fn set_token(
    req: HttpRequest,
    st: web::Data<AppState>,
    body: web::Json<SetCredentialsBody>,
) -> Result<HttpResponse, ApiError> {
    check_key(&req, &st)?;
    if body.token.trim().is_empty() {
        return Err(ApiError::BadRequest("token is required".into()));
    }
    sqlx::query(
        r#"INSERT INTO petroapp_credentials (id, token, cookie, status, updated_at)
           VALUES (1, $1, $2, 'unknown', now())
           ON CONFLICT (id) DO UPDATE
             SET token = EXCLUDED.token, cookie = EXCLUDED.cookie,
                 status = 'unknown', updated_at = now()"#,
    )
    .bind(&body.token)
    .bind(&body.cookie)
    .execute(&st.pool)
    .await?;
    Ok(HttpResponse::Ok().json(json!({ "status": "stored" })))
}

async fn sync_trigger(
    req: HttpRequest,
    st: web::Data<AppState>,
) -> Result<HttpResponse, ApiError> {
    check_key(&req, &st)?;
    let outcome = crate::sync::run_sync(st.get_ref(), "manual").await;
    Ok(HttpResponse::Ok().json(outcome))
}

async fn sync_runs(st: web::Data<AppState>, q: web::Query<RunsQuery>) -> Result<HttpResponse, ApiError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let runs: Vec<SyncRun> = sqlx::query_as::<_, SyncRun>(
        "SELECT id, trigger, status, pages_fetched, bills_fetched, events_inserted, \
         unmapped_count, anomaly_count, error, started_at, finished_at \
         FROM petroapp_sync_runs ORDER BY started_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;
    Ok(HttpResponse::Ok().json(runs))
}

#[derive(serde::Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
}

async fn list_vehicle_map(st: web::Data<AppState>) -> Result<HttpResponse, ApiError> {
    let rows: Vec<VehicleMapRow> = sqlx::query_as::<_, VehicleMapRow>(
        "SELECT id, petroapp_vehicle_id, petroapp_plate, db_car_id, active \
         FROM petroapp_vehicle_map ORDER BY petroapp_vehicle_id",
    )
    .fetch_all(&st.pool)
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

async fn upsert_vehicle_map(
    req: HttpRequest,
    st: web::Data<AppState>,
    body: web::Json<UpsertVehicleBody>,
) -> Result<HttpResponse, ApiError> {
    check_key(&req, &st)?;

    // Ensure the target car exists (FK would catch it too, but give a clean error).
    let car_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cars WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(body.db_car_id)
    .fetch_one(&st.pool)
    .await?;
    if !car_exists {
        return Err(ApiError::BadRequest(format!(
            "no active car with id {}",
            body.db_car_id
        )));
    }

    // Upsert keyed on petroapp_vehicle_id (active rows are unique on it).
    sqlx::query(
        r#"INSERT INTO petroapp_vehicle_map (petroapp_vehicle_id, petroapp_plate, db_car_id, active, updated_at)
           VALUES ($1, $2, $3, $4, now())
           ON CONFLICT (petroapp_vehicle_id) WHERE active DO UPDATE
             SET petroapp_plate = EXCLUDED.petroapp_plate,
                 db_car_id = EXCLUDED.db_car_id,
                 active = EXCLUDED.active,
                 updated_at = now()"#,
    )
    .bind(body.petroapp_vehicle_id)
    .bind(&body.petroapp_plate)
    .bind(body.db_car_id)
    .bind(body.active)
    .execute(&st.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({ "status": "saved" })))
}

async fn deactivate_vehicle_map(
    req: HttpRequest,
    st: web::Data<AppState>,
    path: web::Path<i64>,
) -> Result<HttpResponse, ApiError> {
    check_key(&req, &st)?;
    let petroapp_vehicle_id = path.into_inner();
    let res = sqlx::query(
        "UPDATE petroapp_vehicle_map SET active = false, updated_at = now() \
         WHERE petroapp_vehicle_id = $1 AND active",
    )
    .bind(petroapp_vehicle_id)
    .execute(&st.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(HttpResponse::Ok().json(json!({ "status": "deactivated" })))
}

/// The migrated odometer push-back hook. Falcon calls this on manual fuel events.
async fn odometer_push(
    req: HttpRequest,
    st: web::Data<AppState>,
    body: web::Json<OdometerPushBody>,
) -> Result<HttpResponse, ApiError> {
    check_key(&req, &st)?;

    let petroapp_vehicle_id: Option<i64> = sqlx::query_scalar(
        "SELECT petroapp_vehicle_id FROM petroapp_vehicle_map WHERE db_car_id = $1 AND active",
    )
    .bind(body.db_car_id)
    .fetch_optional(&st.pool)
    .await?;

    let Some(vehicle_id) = petroapp_vehicle_id else {
        return Err(ApiError::BadRequest(format!(
            "car {} has no active PetroApp mapping",
            body.db_car_id
        )));
    };

    let creds: Credentials = sqlx::query_as::<_, Credentials>(
        "SELECT token, cookie, status, last_validated_at, updated_at FROM petroapp_credentials WHERE id = 1",
    )
    .fetch_optional(&st.pool)
    .await?
    .ok_or(ApiError::NoCredentials)?;

    match st.petro.push_odometer(&creds, vehicle_id, body.odometer).await {
        Ok(()) => Ok(HttpResponse::Ok().json(json!({ "status": "pushed" }))),
        Err(crate::error::PetroError::Auth) => {
            let _ = sqlx::query(
                "UPDATE petroapp_credentials SET status = 'expired', updated_at = now() WHERE id = 1",
            )
            .execute(&st.pool)
            .await;
            crate::notify::alert(
                &st.cfg,
                &st.http,
                "token/cookie expired during odometer push — re-login and POST /token.",
            )
            .await;
            Err(ApiError::Petro(crate::error::PetroError::Auth))
        }
        Err(e) => Err(ApiError::Petro(e)),
    }
}
