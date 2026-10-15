//! Thin blocking HTTP client over `reqwest::blocking` for the subset of
//! Forgejo endpoints used by this MCP server. Errors come from
//! [`crate::error::classify_response`].

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use log::debug;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::HeaderValue;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::error::classify_response;
use crate::redact::SecretSet;

/// Single HTTP client shared across all tools. Cheap to clone
/// (`reqwest::blocking::Client` clones share a connection pool). The
/// `SecretString` token redacts on `Debug` / `Display`.
///
/// The `secrets` field is passed to `classify_response` so that any byte
/// echoing the PAT in a Forgejo or proxy error body gets scrubbed before
/// the error string is embedded in an `anyhow::Error` that would otherwise
/// ship unredacted on stdout via the MCP `isError` content channel.
#[derive(Clone)]
pub struct ForgejoClient {
    http: Client,
    base_url: String,
    token: SecretString,
    secrets: SecretSet,
}

impl ForgejoClient {
    /// Build a client. `base_url` should already have any trailing slash
    /// stripped (config loader does this). `secrets` is the shared set of
    /// byte sequences that the error-string scrubber knows about (the PAT
    /// at minimum).
    pub fn new(
        base_url: String,
        token: SecretString,
        user_agent: String,
        timeout_secs: u64,
        secrets: SecretSet,
    ) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent(user_agent)
            // No proxy: we don't want our Authorization-bearing request to
            // pass through HTTPS_PROXY / HTTP_PROXY operators that the
            // deployment may not have vetted. If we ever need proxying,
            // add an explicit `proxy_url` config knob.
            .no_proxy()
            .build()
            .context("building reqwest client")?;
        Ok(Self {
            http,
            base_url,
            token,
            secrets,
        })
    }

    /// POST `/api/v1/repos/{owner}/{repo}/pulls`.
    pub fn create_pull(&self, owner: &str, repo: &str, body: &Value) -> Result<Value> {
        let url = format!("{}/api/v1/repos/{owner}/{repo}/pulls", self.base_url);
        let req = self.http.post(&url).json(body);
        self.send(req, "POST", &url)
    }

    /// GET `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews?page={page}&limit={limit}`.
    /// Returns the raw JSON array. Caller handles pagination.
    pub fn list_reviews(
        &self,
        owner: &str,
        repo: &str,
        index: u64,
        page: u32,
        limit: u32,
    ) -> Result<Vec<Value>> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews",
            self.base_url
        );
        let req = self
            .http
            .get(&url)
            .query(&[("page", page.to_string()), ("limit", limit.to_string())]);
        let v = self.send(req, "GET", &url)?;
        array_or_panic(v, &url)
    }

    /// GET `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews/{id}/comments`.
    /// Per swagger this endpoint does **not** paginate; a single request
    /// returns all comments (capped by Forgejo's `MAX_RESPONSE_ITEMS`).
    pub fn list_review_comments(
        &self,
        owner: &str,
        repo: &str,
        index: u64,
        review_id: u64,
    ) -> Result<Vec<Value>> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews/{review_id}/comments",
            self.base_url
        );
        let req = self.http.get(&url);
        let v = self.send(req, "GET", &url)?;
        array_or_panic(v, &url)
    }

    /// POST `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews`.
    pub fn create_review(
        &self,
        owner: &str,
        repo: &str,
        index: u64,
        body: &Value,
    ) -> Result<Value> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews",
            self.base_url
        );
        let req = self.http.post(&url).json(body);
        self.send(req, "POST", &url)
    }

    /// POST `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews/{review_id}/comments`.
    pub fn create_review_comment(
        &self,
        owner: &str,
        repo: &str,
        index: u64,
        review_id: u64,
        body: &Value,
    ) -> Result<Value> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews/{review_id}/comments",
            self.base_url
        );
        let req = self.http.post(&url).json(body);
        self.send(req, "POST", &url)
    }

    /// PATCH `/api/v1/repos/{owner}/{repo}/pulls/{index}`.
    pub fn patch_pull(&self, owner: &str, repo: &str, index: u64, body: &Value) -> Result<Value> {
        let url = format!(
            "{}/api/v1/repos/{owner}/{repo}/pulls/{index}",
            self.base_url
        );
        let req = self.http.patch(&url).json(body);
        self.send(req, "PATCH", &url)
    }

    /// Common send path: attach auth / accept headers, log at debug,
    /// classify the response. Note: `User-Agent` is set once on the
    /// reqwest `ClientBuilder` and auto-attached to every request; we
    /// don't set it per-request.
    ///
    /// `{owner}/{repo}` are interpolated raw into the URL. Forgejo slugs
    /// are restricted enough (no slashes, no question marks, no `%`) that
    /// URL-encoding is a no-op in practice; we rely on Forgejo to 404
    /// anything exotic rather than pre-validate.
    fn send(&self, req: RequestBuilder, method: &str, url: &str) -> Result<Value> {
        debug!("{method} {url}");
        // Keep this as the only expose_secret() call site. `HeaderValue` is
        // marked sensitive so any `Debug`/`Display` on it prints `Sensitive`
        // instead of the bytes — defense-in-depth against logging.
        let mut auth = HeaderValue::from_str(&format!("token {}", self.token.expose_secret()))
            .map_err(|e| anyhow!("invalid token bytes for Authorization header: {e}"))?;
        auth.set_sensitive(true);
        let resp = req
            .header("Authorization", auth)
            .header("Accept", "application/json")
            .send()
            .map_err(|e| classify_send_error(e, method, url))?;
        let status = resp.status();
        debug!("{method} {url} -> {status}");
        classify_response(resp, &self.secrets)
    }
}

/// A send-phase reqwest error (DNS failure, connect timeout, TLS error,
/// read timeout before the response starts) is a transient error — we
/// haven't seen a status code yet. Surface with the transient prefix.
fn classify_send_error(e: reqwest::Error, method: &str, url: &str) -> anyhow::Error {
    anyhow!("Forgejo transient error: {method} {url}: {e}")
}

/// Coerce a `Value` we expected to be a JSON array into `Vec<Value>`.
/// A non-array body on a 2xx from a list endpoint is an API contract
/// violation; panic rather than return wrong-shaped data.
fn array_or_panic(v: Value, url: &str) -> Result<Vec<Value>> {
    match v {
        Value::Array(arr) => Ok(arr),
        other => panic!(
            "protocol break: expected JSON array from {url}, got: {}",
            value_kind_name(&other)
        ),
    }
}

fn value_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
