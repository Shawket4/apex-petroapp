use std::time::Duration;

/// Runtime configuration, loaded from environment variables at startup.
#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,

    /// PetroApp dashboard API base, e.g. https://app.petroappeg.com/dashboard_api
    pub petroapp_base_url: String,

    /// How often the background sync runs.
    pub sync_interval: Duration,
    /// How many days back the `bills` query reaches.
    pub sync_lookback_days: i64,

    /// Efficiency (km/L) outside [min, max] is flagged as an anomaly.
    pub anomaly_rate_min: f64,
    pub anomaly_rate_max: f64,

    /// Per-request HTTP timeout and retry budget for transient failures.
    pub http_timeout: Duration,
    pub max_retries: u32,

    /// Optional shared secret. When set, mutating endpoints require the
    /// `X-Api-Key` header to match. Falcon sends this when calling /odometer/push.
    pub internal_api_key: Option<String>,

    /// Optional Slack-compatible incoming webhook for operational alerts.
    pub slack_webhook_url: Option<String>,

    /// Internal WhatsApp Go service base URL (e.g. http://localhost:3000).
    /// Messages are POSTed to `{url}/send/message` with {phone,message}.
    pub whatsapp_service_url: Option<String>,
    /// WhatsApp recipient id (e.g. the fleet fuel group "....@g.us").
    pub whatsapp_fuel_group_id: Option<String>,
    /// If a single run inserts more than this many events, send one summary
    /// message instead of per-event spam (e.g. a first backfill).
    pub notify_per_event_max: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        // Best-effort .env load; real env always wins.
        let _ = dotenvy::dotenv();

        let database_url = req("DATABASE_URL")?;

        let cfg = Config {
            database_url,
            bind_addr: opt("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8090".to_string()),
            petroapp_base_url: opt("PETROAPP_BASE_URL")
                .unwrap_or_else(|| "https://app.petroappeg.com/dashboard_api".to_string()),
            sync_interval: Duration::from_secs(parse_or("SYNC_INTERVAL_SECS", 300)),
            sync_lookback_days: parse_or("SYNC_LOOKBACK_DAYS", 14) as i64,
            anomaly_rate_min: parse_or_f("ANOMALY_RATE_MIN", 1.9),
            anomaly_rate_max: parse_or_f("ANOMALY_RATE_MAX", 2.8),
            http_timeout: Duration::from_secs(parse_or("HTTP_TIMEOUT_SECS", 30)),
            max_retries: parse_or("MAX_RETRIES", 3) as u32,
            internal_api_key: opt("INTERNAL_API_KEY").filter(|s| !s.is_empty()),
            slack_webhook_url: opt("SLACK_WEBHOOK_URL").filter(|s| !s.is_empty()),
            whatsapp_service_url: opt("WHATSAPP_SERVICE_URL").filter(|s| !s.is_empty()),
            whatsapp_fuel_group_id: opt("WHATSAPP_FUEL_GROUP_ID").filter(|s| !s.is_empty()),
            notify_per_event_max: parse_or("NOTIFY_PER_EVENT_MAX", 15) as usize,
        };

        if cfg.anomaly_rate_min >= cfg.anomaly_rate_max {
            anyhow::bail!("ANOMALY_RATE_MIN must be < ANOMALY_RATE_MAX");
        }
        if cfg.internal_api_key.is_none() {
            tracing::warn!(
                "INTERNAL_API_KEY is not set — mutating endpoints are UNAUTHENTICATED. \
                 Set it in production so only Falcon can call /odometer/push etc."
            );
        }
        Ok(cfg)
    }
}

fn req(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("missing required env var: {key}"))
}
fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
fn parse_or(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn parse_or_f(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
