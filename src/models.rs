use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Stored PetroApp credentials.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Credentials {
    pub token: String,
    pub cookie: String,
    pub status: String,
    pub last_validated_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// A vehicle-map entry.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct VehicleMapRow {
    pub id: i64,
    pub petroapp_vehicle_id: i64,
    pub petroapp_plate: Option<String>,
    pub db_car_id: i64,
    pub active: bool,
}

/// A sync-run audit row.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct SyncRun {
    pub id: i64,
    pub trigger: String,
    pub status: String,
    pub pages_fetched: i32,
    pub bills_fetched: i32,
    pub events_inserted: i32,
    pub unmapped_count: i32,
    pub anomaly_count: i32,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Row returned by the conversion INSERT ... RETURNING, used for notifications.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InsertedEvent {
    pub car_no_plate: String,
    pub date: String,
    pub time: String,
    pub odometer_after: i64,
    pub liters: f64,
    pub fuel_rate: f64,
    pub petroapp_bill_id: i64,
}

// ---- API request/response bodies ----

#[derive(Debug, Deserialize)]
pub struct SetCredentialsBody {
    pub token: String,
    #[serde(default)]
    pub cookie: String,
}

#[derive(Debug, Deserialize)]
pub struct UpsertVehicleBody {
    pub petroapp_vehicle_id: i64,
    pub db_car_id: i64,
    #[serde(default)]
    pub petroapp_plate: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
}
fn default_true() -> bool {
    true
}

/// Body for the migrated odometer push-back hook. Falcon sends the internal car
/// id (preferred); the service resolves the PetroApp vehicle id from the map.
#[derive(Debug, Deserialize)]
pub struct OdometerPushBody {
    pub db_car_id: i64,
    pub odometer: i64,
}
