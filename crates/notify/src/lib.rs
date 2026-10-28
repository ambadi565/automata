//! Telegram Bot API, just what Saarthi needs: send text, a PNG or a file to
//! the owner's chat, and long-poll for commands from that chat only.
//!
//! Configured by `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` (environment
//! first, then the secrets file). The token never appears in logs or errors.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

/// Telegram's limit for one message.
pub const MAX_TEXT: usize = 4096;

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("telegram request failed: {0}")]
    Http(String),
    #[error("telegram refused: {0}")]
    Api(String),
}

pub type Result<T> = std::result::Result<T, NotifyError>;

#[derive(Clone)]
pub struct Telegram {
    client: reqwest::Client,
    base: String,
    chat_id: String,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

/// One command from the owner's chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub update_id: i64,
    /// Unix seconds.
    pub date: i64,
    /// Lowercase, without the slash or `@botname`.
    pub name: String,
    pub args: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    date: i64,
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
}

fn secret(path: Option<&Path>, key: &str) -> Option<String> {
    let v = std::env::var(key).ok().filter(|v| !v.trim().is_empty()).or_else(|| {
        dotenvy::from_path_iter(path?)
            .ok()?
            .flatten()
            .find(|(k, v)| k == key && !v.trim().is_empty())
            .map(|(_, v)| v)
    })?;
    Some(v.trim().to_string())
}

/// Splits `/cmd@bot args` into `("cmd", "args")`.
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    let rest = t.strip_prefix('/')?;
    let (head, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = head.split('@').next().unwrap_or("").to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name, args.trim().to_string()))
}

/// Cuts `s` to Telegram's limit on a character boundary.
pub fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_TEXT - 20).collect();
    out.push_str("\n…(truncated)");
    out
}

impl Telegram {
    pub fn new(token: &str, chat_id: &str) -> Result<Self> {
        Self::with_base("https://api.telegram.org", token, chat_id)
    }

    /// For tests: a different API host.
    pub fn with_base(base: &str, token: &str, chat_id: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        Ok(Telegram {
            client,
            base: format!("{}/bot{token}", base.trim_end_matches('/')),
            chat_id: chat_id.trim().to_string(),
        })
    }

    /// `None` unless both the bot token and chat id are set.
    pub fn from_secrets(path: Option<&Path>) -> Option<Self> {
        let token = secret(path, "TELEGRAM_BOT_TOKEN")?;
        let chat = secret(path, "TELEGRAM_CHAT_ID")?;
        Telegram::new(&token, &chat).ok()
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        let r = req
            .send()
            .await
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        let env: Envelope<T> = r
            .json()
            .await
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        match (env.ok, env.result) {
            (true, Some(v)) => Ok(v),
            _ => Err(NotifyError::Api(env.description.unwrap_or_else(|| "no result".into()))),
        }
    }

    /// Plain text (no parse mode, so nothing in it can break formatting).
    pub async fn send_text(&self, text: &str) -> Result<()> {
        let body = json!({ "chat_id": self.chat_id, "text": clip(text), "disable_web_page_preview": true });
        self.call::<Value>(self.client.post(format!("{}/sendMessage", self.base)).json(&body))
            .await
            .map(|_| ())
    }

    pub async fn send_photo(&self, png: Vec<u8>, caption: &str) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(png)
            .file_name("chart.png")
            .mime_str("image/png")
            .map_err(|e| NotifyError::Http(e.to_string()))?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", self.chat_id.clone())
            .text("caption", caption.chars().take(1000).collect::<String>())
            .part("photo", part);
        self.call::<Value>(self.client.post(format!("{}/sendPhoto", self.base)).multipart(form))
            .await
            .map(|_| ())
    }

    pub async fn send_document(&self, name: &str, bytes: Vec<u8>, mime: &str, caption: &str) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .mime_str(mime)
            .map_err(|e| NotifyError::Http(e.to_string()))?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", self.chat_id.clone())
            .text("caption", caption.chars().take(1000).collect::<String>())
            .part("document", part);
        self.call::<Value>(self.client.post(format!("{}/sendDocument", self.base)).multipart(form))
            .await
            .map(|_| ())
    }

    /// Long-polls for updates after `offset`. Returns the commands sent from
    /// the configured chat (everything else is ignored) and the next offset.
    pub async fn commands(&self, offset: i64, wait_secs: u64) -> Result<(Vec<Command>, i64)> {
        let body = json!({ "offset": offset, "timeout": wait_secs, "allowed_updates": ["message"] });
        let updates: Vec<Update> = self
            .call(
                self.client
                    .post(format!("{}/getUpdates", self.base))
                    .timeout(Duration::from_secs(wait_secs + 15))
                    .json(&body),
            )
            .await?;
        let mut next = offset;
        let mut out = Vec::new();
        for u in updates {
            next = next.max(u.update_id + 1);
            let Some(m) = u.message else { continue };
            if m.chat.id.to_string() != self.chat_id {
                tracing::warn!(chat = m.chat.id, "ignored a command from an unknown chat");
                continue;
            }
            if let Some((name, args)) = m.text.as_deref().and_then(parse_command) {
                out.push(Command {
                    update_id: u.update_id,
                    date: m.date,
                    name,
                    args,
                });
            }
        }
        Ok((out, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_command("/halt news spike"),
            Some(("halt".into(), "news spike".into()))
        );
        assert_eq!(parse_command("/PnL@SaarthiBot"), Some(("pnl".into(), String::new())));
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("/"), None);
        assert_eq!(clip(&"x".repeat(5000)).chars().count(), MAX_TEXT - 7);
    }

    #[tokio::test]
    async fn only_the_owner_chat_is_obeyed() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botTOKEN/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": [
                {"update_id": 10, "message": {"date": 1, "chat": {"id": 42}, "text": "/status"}},
                {"update_id": 11, "message": {"date": 2, "chat": {"id": 666}, "text": "/resume"}},
                {"update_id": 12, "message": {"date": 3, "chat": {"id": 42}, "text": "just chatting"}},
            ]})))
            .mount(&srv)
            .await;
        let t = Telegram::with_base(&srv.uri(), "TOKEN", "42").unwrap();
        let (cmds, next) = t.commands(0, 0).await.unwrap();
        assert_eq!(next, 13);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "status");
    }

    #[tokio::test]
    async fn api_errors_do_not_leak_the_token() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"ok": false, "description": "chat not found"})),
            )
            .mount(&srv)
            .await;
        let t = Telegram::with_base(&srv.uri(), "SECRET123", "42").unwrap();
        let e = t.send_text("hi").await.unwrap_err().to_string();
        assert!(e.contains("chat not found") && !e.contains("SECRET123"), "{e}");
        let e = t.send_photo(vec![1, 2, 3], "c").await.unwrap_err().to_string();
        assert!(!e.contains("SECRET123"));
    }
}
