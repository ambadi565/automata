//! Where the engine's alerts go: always the log, plus Telegram when a bot
//! token and chat id are configured. Sending never blocks trading.

use std::sync::Arc;
use std::time::Duration;

use saarthi_oms::Severity;

pub trait Alerts: Send + Sync {
    fn send(&self, severity: Severity, text: &str);
}

/// Log only.
pub struct LogAlerts;

impl Alerts for LogAlerts {
    fn send(&self, severity: Severity, text: &str) {
        match severity {
            Severity::Critical => tracing::error!(alert = text),
            Severity::Warning => tracing::warn!(alert = text),
            Severity::Info => tracing::info!(alert = text),
        }
    }
}

/// Collects alerts in memory (tests, replays).
#[derive(Default)]
pub struct MemoryAlerts(pub std::sync::Mutex<Vec<(Severity, String)>>);

impl Alerts for MemoryAlerts {
    fn send(&self, severity: Severity, text: &str) {
        LogAlerts.send(severity, text);
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((severity, text.to_string()));
    }
}

/// Telegram Bot API `sendMessage`, fire and forget, plus the log.
pub struct TelegramAlerts {
    client: reqwest::Client,
    url: String,
    chat_id: String,
    label: String,
}

impl TelegramAlerts {
    pub fn new(token: &str, chat_id: &str, label: &str) -> anyhow::Result<Self> {
        Ok(TelegramAlerts {
            client: reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?,
            url: format!("https://api.telegram.org/bot{token}/sendMessage"),
            chat_id: chat_id.to_string(),
            label: label.to_string(),
        })
    }

    /// From `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` in the secrets file or
    /// the environment; `None` if either is missing.
    pub fn from_secrets(path: Option<&std::path::Path>, label: &str) -> Option<Self> {
        let mut token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        let mut chat = std::env::var("TELEGRAM_CHAT_ID").ok();
        if let Some(p) = path
            && let Ok(iter) = dotenvy::from_path_iter(p)
        {
            for (k, v) in iter.flatten() {
                match k.as_str() {
                    "TELEGRAM_BOT_TOKEN" if token.is_none() => token = Some(v),
                    "TELEGRAM_CHAT_ID" if chat.is_none() => chat = Some(v),
                    _ => {}
                }
            }
        }
        let (token, chat) = (
            token.filter(|s| !s.trim().is_empty())?,
            chat.filter(|s| !s.trim().is_empty())?,
        );
        TelegramAlerts::new(token.trim(), chat.trim(), label).ok()
    }
}

impl Alerts for TelegramAlerts {
    fn send(&self, severity: Severity, text: &str) {
        LogAlerts.send(severity, text);
        let icon = match severity {
            Severity::Critical => "🚨",
            Severity::Warning => "⚠️",
            Severity::Info => "ℹ️",
        };
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": format!("{icon} [{}] {text}", self.label),
            "disable_web_page_preview": true,
        });
        let req = self.client.post(&self.url).json(&body);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                match req.send().await {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => tracing::warn!(status = %r.status(), "telegram alert refused"),
                    Err(e) => tracing::warn!(error = %e, "telegram alert failed"),
                }
            });
        }
    }
}

pub type SharedAlerts = Arc<dyn Alerts>;
