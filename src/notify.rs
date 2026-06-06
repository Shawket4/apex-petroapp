use crate::config::Config;

/// Fire-and-forget Slack-compatible operational alert (token expired, run failed,
/// unmapped vehicles). Never errors out to the caller — logs on failure.
pub async fn alert(cfg: &Config, http: &reqwest::Client, text: &str) {
    tracing::warn!(alert = %text, "petroapp alert");
    if let Some(url) = &cfg.slack_webhook_url {
        let payload = serde_json::json!({ "text": format!(":fuelpump: PetroApp: {text}") });
        if let Err(e) = http.post(url).json(&payload).send().await {
            tracing::error!(error = %e, "failed to deliver slack alert");
        }
    }
    // Operational alerts also go to the WhatsApp fleet group when configured.
    whatsapp(cfg, http, &format!("\u{26A0}\u{FE0F} PetroApp: {text}")).await;
}

/// Send a WhatsApp message via Falcon's existing internal WhatsApp Go service:
/// POST {WHATSAPP_SERVICE_URL}/send/message  body {"phone","message"}.
/// Best-effort; failures are logged, never propagated.
pub async fn whatsapp(cfg: &Config, http: &reqwest::Client, message: &str) {
    let (Some(base), Some(group)) = (&cfg.whatsapp_service_url, &cfg.whatsapp_fuel_group_id)
    else {
        return;
    };
    let url = format!("{}/send/message", base.trim_end_matches('/'));
    let payload = serde_json::json!({ "phone": group, "message": message });
    if let Err(e) = http.post(&url).json(&payload).send().await {
        tracing::error!(error = %e, "failed to deliver whatsapp message");
    }
}
