//! Response classifier: one function that turns a `reqwest` response into
//! either a parsed JSON value, a classified `Err`, or a panic.

use anyhow::{Result, anyhow};
use reqwest::blocking::Response;
use serde_json::Value;

use crate::redact::{SecretSet, scrub_str};

/// Leading text of every transient-error message. Callers that classify
/// errors by kind match on this prefix.
pub(crate) const TRANSIENT_PREFIX: &str = "Forgejo transient error:";

/// Peel a `reqwest::blocking::Response` into one of three outcomes:
///
/// - 2xx with valid JSON → `Ok(Value)`.
/// - 2xx with a body that is not valid JSON → **panic** (protocol break).
/// - 4xx → `Err(anyhow!("Forgejo API error ({status}): {msg}"))` where `msg`
///   is the `message` field of Forgejo's error JSON (if any), else the raw
///   body truncated. The message is scrubbed against `secrets` before being
///   embedded — defense-in-depth against a misbehaving proxy that echoes
///   request headers into the body.
/// - 5xx / anything else non-success → `Err(anyhow!("Forgejo transient
///   error: HTTP {status}"))`. The body is deliberately NOT embedded: 5xx
///   bodies are most likely upstream proxy / WAF error pages, which are
///   the most realistic vehicle for a PAT-echo leak. Status code alone is
///   enough for Claude's retry decision (the prefix is what matters).
pub fn classify_response(resp: Response, secrets: &SecretSet) -> Result<Value> {
    let status = resp.status();
    let body_bytes = resp
        .bytes()
        .map_err(|e| anyhow!("{TRANSIENT_PREFIX} reading response body: {e}"))?;

    if status.is_success() {
        // 2xx: body must be valid JSON; anything else means the API shape
        // we expect is no longer true. Panic rather than return wrong data.
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(v) => Ok(v),
            Err(e) => panic!(
                "protocol break: Forgejo returned 2xx ({status}) with a body that is not valid JSON: {e}"
            ),
        }
    } else if status.is_client_error() {
        let raw = extract_error_message(&body_bytes);
        let msg = scrub_str(&raw, secrets);
        Err(anyhow!("Forgejo API error ({status}): {msg}"))
    } else {
        // 5xx and anything else non-success: transient. Status only, no
        // body preview (see doc comment).
        Err(anyhow!("{TRANSIENT_PREFIX} HTTP {status}"))
    }
}

/// Extract the `message` field from Forgejo's error JSON shape
/// `{"message": "...", "url": "..."}`. Falls back to the raw body
/// (truncated) if the body is not JSON or lacks `message`.
fn extract_error_message(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body)
        && let Some(msg) = v.get("message").and_then(|m| m.as_str())
    {
        return msg.to_string();
    }
    truncate(&String::from_utf8_lossy(body), 512).into_owned()
}

fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.len() <= max {
        std::borrow::Cow::Borrowed(s)
    } else {
        // Back off to the nearest UTF-8 char boundary ≤ max so slicing
        // never panics on a multi-byte character that straddles the cap.
        let cut = s.floor_char_boundary(max);
        std::borrow::Cow::Owned(format!("{}… (truncated)", &s[..cut]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_error_message_from_forgejo_shape() {
        let body = br#"{"message": "invalid head/base", "url": "https://..."}"#;
        assert_eq!(extract_error_message(body), "invalid head/base");
    }

    #[test]
    fn extract_error_message_falls_back_to_raw() {
        let body = b"<html>nginx error</html>";
        assert!(extract_error_message(body).contains("nginx"));
    }

    #[test]
    fn extract_error_message_no_message_field() {
        let body = br#"{"other": "field"}"#;
        assert!(extract_error_message(body).contains("other"));
    }

    #[test]
    fn truncate_short_is_borrowed() {
        let s = "hello";
        assert_eq!(&*truncate(s, 10), "hello");
    }

    #[test]
    fn truncate_long_is_cut() {
        let s = "x".repeat(1000);
        let t = truncate(&s, 32);
        assert!(t.len() < 100);
        assert!(t.contains("truncated"));
    }

    #[test]
    fn truncate_handles_non_ascii_boundary() {
        // The em-dash '—' is 3 UTF-8 bytes. Build a string where slicing
        // at `max` would land in the middle of that codepoint and check
        // that `truncate` backs off instead of panicking.
        let s = format!("{}—tail", "a".repeat(10));
        let t = truncate(&s, 11);
        assert!(t.contains("truncated"));
        assert!(t.starts_with("aaaaaaaaaa…"), "got: {t}");
    }
}
