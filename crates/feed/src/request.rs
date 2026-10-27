//! JSON control messages: subscribe/unsubscribe requests and the server's
//! text replies.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wire::{ExchangeType, Mode};

/// Text heartbeat. Angel expects one at least every 30 s and answers `pong`.
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Unsubscribe = 0,
    Subscribe = 1,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenList<'a> {
    exchange_type: u8,
    tokens: Vec<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Params<'a> {
    mode: u8,
    token_list: Vec<TokenList<'a>>,
}

#[derive(Serialize)]
struct Request<'a> {
    #[serde(rename = "correlationID")]
    correlation_id: &'a str,
    action: u8,
    params: Params<'a>,
}

/// Builds one request for `tokens`, grouped by exchange type in a stable order.
pub fn request_json(correlation_id: &str, action: Action, mode: Mode, tokens: &[(ExchangeType, String)]) -> String {
    let mut by_ex: BTreeMap<ExchangeType, Vec<&str>> = BTreeMap::new();
    for (ex, t) in tokens {
        by_ex.entry(*ex).or_default().push(t);
    }
    let req = Request {
        correlation_id,
        action: action as u8,
        params: Params {
            mode: mode.code(),
            token_list: by_ex
                .into_iter()
                .map(|(ex, tokens)| TokenList {
                    exchange_type: ex.code(),
                    tokens,
                })
                .collect(),
        },
    };
    serde_json::to_string(&req).expect("request serializes")
}

/// A text frame from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerText {
    Pong,
    Error { code: String, message: String },
    Other(String),
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(rename = "errorCode", default)]
    code: String,
    #[serde(rename = "errorMessage", default)]
    message: String,
}

pub fn parse_server_text(s: &str) -> ServerText {
    let t = s.trim();
    if t.eq_ignore_ascii_case(PONG) {
        return ServerText::Pong;
    }
    match serde_json::from_str::<ErrorBody>(t) {
        Ok(e) if !e.code.is_empty() || !e.message.is_empty() => ServerText::Error {
            code: e.code,
            message: e.message,
        },
        _ => ServerText::Other(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_request_matches_documented_shape() {
        let tokens = vec![
            (ExchangeType::NseFo, "35003".to_string()),
            (ExchangeType::NseCm, "10626".to_string()),
            (ExchangeType::NseCm, "5290".to_string()),
        ];
        let got: serde_json::Value =
            serde_json::from_str(&request_json("abcde12345", Action::Subscribe, Mode::Quote, &tokens)).unwrap();
        let want = serde_json::json!({
            "correlationID": "abcde12345",
            "action": 1,
            "params": {
                "mode": 2,
                "tokenList": [
                    {"exchangeType": 1, "tokens": ["10626", "5290"]},
                    {"exchangeType": 2, "tokens": ["35003"]}
                ]
            }
        });
        assert_eq!(got, want);
        let unsub = request_json("x", Action::Unsubscribe, Mode::Ltp, &tokens[..1]);
        assert!(unsub.contains(r#""action":0"#));
    }

    #[test]
    fn parses_server_replies() {
        assert_eq!(parse_server_text("pong"), ServerText::Pong);
        assert_eq!(
            parse_server_text(
                r#"{"correlationID":"abc","errorCode":"E1002","errorMessage":"Invalid Request Payload."}"#
            ),
            ServerText::Error {
                code: "E1002".into(),
                message: "Invalid Request Payload.".into()
            }
        );
        assert_eq!(parse_server_text("hello"), ServerText::Other("hello".into()));
    }
}
