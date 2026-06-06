use std::time::Duration;

use chrono::Utc;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::sleep;

use crate::config::Config;
use crate::error::PetroError;
use crate::models::Credentials;

/// Field names inside a PetroApp bill payload. Centralised so they can be
/// confirmed against a real response in one place. (See README open items.)
pub const FIELD_ID: &str = "id";
pub const FIELD_VEHICLE_PLATE: &str = "vehicle";

/// Date format used in the `dates=START-END` query parameter.
const DATE_FMT: &str = "%Y-%m-%d";

#[derive(Clone)]
pub struct PetroClient {
    http: reqwest::Client,
    base_url: String,
    max_retries: u32,
}

/// Result of a bills pull: the raw bill objects plus how many pages were read.
pub struct BillsResult {
    pub bills: Vec<Value>,
    pub pages: u32,
}

impl PetroClient {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.http_timeout)
            .user_agent("Falcon-PetroApp-Sync/2.0")
            .build()?;
        Ok(Self {
            http,
            base_url: cfg.petroapp_base_url.trim_end_matches('/').to_string(),
            max_retries: cfg.max_retries,
        })
    }

    /// Fetch every page of bills from `start` through today (inclusive). Returns
    /// all bill objects. Authentication failures surface as `PetroError::Auth`.
    pub async fn fetch_bills(
        &self,
        creds: &Credentials,
        start: chrono::NaiveDate,
    ) -> Result<BillsResult, PetroError> {
        let end = Utc::now().date_naive();
        let dates = format!("{}-{}", start.format(DATE_FMT), end.format(DATE_FMT));

        let mut bills = Vec::new();
        let mut page: u32 = 1;
        let mut last_page: u32 = 1;

        loop {
            let url = format!(
                "{}/bills?dates={}&limit=1000&page={}",
                self.base_url, dates, page
            );
            let body = self.get_with_retry(&url, creds).await?;

            // data array
            if let Some(arr) = body.get("data").and_then(Value::as_array) {
                bills.extend(arr.iter().cloned());
            } else {
                return Err(PetroError::Payload("missing `data` array in bills response".into()));
            }

            // pagination meta
            last_page = body
                .get("meta")
                .and_then(|m| m.get("last_page"))
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or(page);

            if page >= last_page {
                break;
            }
            page += 1;
        }

        Ok(BillsResult { bills, pages: last_page })
    }

    /// Push a manual odometer reading back to PetroApp. Mirrors the old Go hook's
    /// POST to `/edit_odometer`.
    pub async fn push_odometer(
        &self,
        creds: &Credentials,
        petroapp_vehicle_id: i64,
        odometer: i64,
    ) -> Result<(), PetroError> {
        let url = format!("{}/edit_odometer", self.base_url);
        let payload = serde_json::json!({
            "vehicle_id": petroapp_vehicle_id,
            "odometer": odometer,
        });

        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self
                .http
                .post(&url)
                .header("Authorization", &creds.token)
                .header("Cookie", &creds.cookie)
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) if is_auth(r.status()) => return Err(PetroError::Auth),
                Ok(r) if r.status().is_server_error() && attempt <= self.max_retries => {
                    backoff(attempt).await;
                }
                Ok(r) => {
                    return Err(PetroError::Request(format!(
                        "edit_odometer returned {}",
                        r.status()
                    )))
                }
                Err(e) if attempt <= self.max_retries => {
                    tracing::warn!(attempt, error = %e, "edit_odometer transient error, retrying");
                    backoff(attempt).await;
                }
                Err(e) => return Err(PetroError::Request(e.to_string())),
            }
        }
    }

    /// GET a URL with auth headers and bounded retries on transient failures.
    async fn get_with_retry(&self, url: &str, creds: &Credentials) -> Result<Value, PetroError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self
                .http
                .get(url)
                .header("Authorization", &creds.token)
                .header("Cookie", &creds.cookie)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    return r
                        .json::<Value>()
                        .await
                        .map_err(|e| PetroError::Payload(e.to_string()));
                }
                Ok(r) if is_auth(r.status()) => return Err(PetroError::Auth),
                Ok(r) if r.status().is_server_error() && attempt <= self.max_retries => {
                    tracing::warn!(attempt, status = %r.status(), "bills 5xx, retrying");
                    backoff(attempt).await;
                }
                Ok(r) => {
                    return Err(PetroError::Request(format!("bills returned {}", r.status())))
                }
                Err(e) if attempt <= self.max_retries => {
                    tracing::warn!(attempt, error = %e, "bills transient error, retrying");
                    backoff(attempt).await;
                }
                Err(e) => return Err(PetroError::Request(e.to_string())),
            }
        }
    }
}

fn is_auth(s: StatusCode) -> bool {
    s == StatusCode::UNAUTHORIZED || s == StatusCode::FORBIDDEN
}

/// Exponential backoff: 0.5s, 1s, 2s, ... capped at 8s.
async fn backoff(attempt: u32) {
    let secs = (0.5_f64 * 2f64.powi(attempt as i32 - 1)).min(8.0);
    sleep(Duration::from_millis((secs * 1000.0) as u64)).await;
}

/// Extract the bill id from a payload, tolerating numeric or string encodings.
pub fn extract_bill_id(bill: &Value) -> Option<i64> {
    match bill.get(FIELD_ID) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Extract the vehicle plate string used to join the vehicle map.
pub fn extract_vehicle_plate(bill: &Value) -> Option<String> {
    bill.get(FIELD_VEHICLE_PLATE)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}
