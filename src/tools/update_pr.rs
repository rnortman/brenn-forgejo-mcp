//! `forgejo_update_pr` — PATCH `/api/v1/repos/{owner}/{repo}/pulls/{index}`
//! with any of `title`, `body`, `base`.
//!
//! Forgejo's edit handler writes `title` and `body` before it evaluates
//! `base`, without a transaction, and silently skips a base change on a
//! merged PR while still answering 201. So `base` must travel alone, and
//! a 201 whose `base.ref` is not the requested one is reported as an error.

use anyhow::{Result, anyhow};
use brenn_mcp::Tool;
use serde_json::{Map, Value, json};

use crate::client::ForgejoClient;
use crate::error::TRANSIENT_PREFIX;
use crate::tools::{optional_str, pr_id_properties, required_str, required_u64};

const SAME_COMMIT_NOTE: &str = "; note: Forgejo also answers 500 when the new base is at the \
     same commit as the PR's head — check that before retrying";

/// MCP tool `forgejo_update_pr`: edits a PR's title, body or base.
pub struct ForgejoUpdatePrTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoUpdatePrTool {
    fn name(&self) -> &str {
        "forgejo_update_pr"
    }

    fn description(&self) -> &str {
        "Edit an existing PR's title, body (description) and/or base (target branch). \
         Provide at least one of title, body, base. title and base must be non-empty; \
         body may be \"\" to clear the description. base cannot be combined with title \
         or body in one call: change the base first, then the title/body in a second call. \
         Returns the updated Forgejo PR object. \
         A base change on a merged PR is refused with an error naming the current and \
         requested base. A 409 means a PR for the new base and this head already exists \
         or the PR has merged; retrying without changes is pointless. Forgejo also answers \
         500 when the new base is at the same commit as the PR's head (e.g. the head \
         branch itself); check that before retrying a 500 on a base change. \
         To close a PR use forgejo_close_pr."
    }

    fn input_schema(&self) -> Value {
        let mut props = pr_id_properties();
        props.insert(
            "title".into(),
            json!({ "type": "string", "description": "New PR title. Non-empty." }),
        );
        props.insert(
            "body".into(),
            json!({
                "type": "string",
                "description": "New markdown PR description. Empty string clears it."
            }),
        );
        props.insert(
            "base".into(),
            json!({
                "type": "string",
                "description": "New target branch name. Non-empty. Cannot be combined with title or body."
            }),
        );
        json!({
            "type": "object",
            "properties": Value::Object(props),
            "required": ["owner", "repo", "index"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let index = required_u64(&args, "index")?;
        let (payload, base) = build_payload(&args)?;

        let pr = self
            .client
            .patch_pull(owner, repo, index, &Value::Object(payload))
            .map_err(|e| annotate_error(e, base.is_some()))?;

        if let Some(requested) = base {
            check_base_applied(&pr, requested)?;
        }
        Ok(pr)
    }
}

/// Validate the editable fields and build the PATCH payload holding only
/// the provided keys. Returns the requested `base`, if any, alongside.
fn build_payload(args: &Value) -> Result<(Map<String, Value>, Option<&str>)> {
    let title = optional_str(args, "title")?;
    let body = optional_str(args, "body")?;
    let base = optional_str(args, "base")?;

    if title.is_none() && body.is_none() && base.is_none() {
        return Err(anyhow!("at least one of title, body or base is required"));
    }
    if title == Some("") {
        return Err(anyhow!("title must not be empty"));
    }
    if base == Some("") {
        return Err(anyhow!("base must not be empty"));
    }
    if base.is_some() && (title.is_some() || body.is_some()) {
        return Err(anyhow!(
            "base cannot be combined with title or body in one call; change the base first, then the title/body"
        ));
    }

    let mut payload = Map::new();
    if let Some(t) = title {
        payload.insert("title".to_string(), json!(t));
    }
    if let Some(b) = body {
        payload.insert("body".to_string(), json!(b));
    }
    if let Some(b) = base {
        payload.insert("base".to_string(), json!(b));
    }
    Ok((payload, base))
}

/// Forgejo answers 201 with the old base when it skips a base change (merged
/// PR). A 201 PR object without a string `base.ref` is a protocol break.
fn check_base_applied(pr: &Value, requested: &str) -> Result<()> {
    let actual = pr
        .get("base")
        .and_then(|b| b.get("ref"))
        .and_then(|r| r.as_str())
        .unwrap_or_else(|| {
            panic!("protocol break: Forgejo PR update response has no string base.ref")
        });
    if actual == requested {
        Ok(())
    } else {
        Err(anyhow!(
            "Forgejo accepted the update but the base is still `{actual}` (requested `{requested}`); base cannot change on a merged PR"
        ))
    }
}

/// Append the same-commit note to a transient error on a base change; any
/// other error passes through unchanged.
fn annotate_error(err: anyhow::Error, base_requested: bool) -> anyhow::Error {
    let msg = format!("{err:#}");
    if base_requested && msg.starts_with(TRANSIENT_PREFIX) {
        anyhow!("{msg}{SAME_COMMIT_NOTE}")
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_text(args: Value) -> String {
        format!("{:#}", build_payload(&args).unwrap_err())
    }

    #[test]
    fn payload_has_only_provided_keys() {
        let args = json!({ "title": "T", "body": null });
        let (p, base) = build_payload(&args).unwrap();
        assert_eq!(Value::Object(p), json!({ "title": "T" }));
        assert_eq!(base, None);
    }

    #[test]
    fn payload_title_and_body() {
        let args = json!({ "title": "T", "body": "B" });
        let (p, _) = build_payload(&args).unwrap();
        assert_eq!(Value::Object(p), json!({ "title": "T", "body": "B" }));
    }

    #[test]
    fn payload_base_only_returns_base() {
        let args = json!({ "base": "dev" });
        let (p, base) = build_payload(&args).unwrap();
        assert_eq!(Value::Object(p), json!({ "base": "dev" }));
        assert_eq!(base, Some("dev"));
    }

    #[test]
    fn empty_body_accepted() {
        let args = json!({ "body": "" });
        let (p, _) = build_payload(&args).unwrap();
        assert_eq!(Value::Object(p), json!({ "body": "" }));
    }

    #[test]
    fn no_fields_rejected() {
        assert_eq!(
            err_text(json!({ "title": null })),
            "at least one of title, body or base is required"
        );
    }

    #[test]
    fn empty_title_rejected() {
        assert_eq!(err_text(json!({ "title": "" })), "title must not be empty");
    }

    #[test]
    fn empty_base_rejected() {
        assert_eq!(err_text(json!({ "base": "" })), "base must not be empty");
    }

    #[test]
    fn base_with_title_or_body_rejected() {
        for args in [
            json!({ "base": "dev", "title": "T" }),
            json!({ "base": "dev", "body": "" }),
        ] {
            assert_eq!(
                err_text(args),
                "base cannot be combined with title or body in one call; change the base first, then the title/body"
            );
        }
    }

    #[test]
    fn non_string_field_rejected() {
        assert_eq!(
            err_text(json!({ "title": 5 })),
            "field title must be a string"
        );
    }

    #[test]
    fn base_check_passes_on_match() {
        check_base_applied(&json!({ "base": { "ref": "dev" } }), "dev").unwrap();
    }

    #[test]
    fn base_check_errors_on_mismatch() {
        let err = check_base_applied(&json!({ "base": { "ref": "main" } }), "dev").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("still `main`"), "got: {s}");
        assert!(s.contains("requested `dev`"), "got: {s}");
    }

    #[test]
    #[should_panic(expected = "protocol break")]
    fn base_check_panics_without_base_ref() {
        let _ = check_base_applied(&json!({ "base": {} }), "dev");
    }

    #[test]
    fn note_appended_only_for_transient_with_base() {
        let transient = || anyhow!("{TRANSIENT_PREFIX} HTTP 500 Internal Server Error");
        let api = || anyhow!("Forgejo API error (409 Conflict): exists");

        let s = format!("{:#}", annotate_error(transient(), true));
        assert!(s.starts_with(TRANSIENT_PREFIX), "got: {s}");
        assert!(s.ends_with(SAME_COMMIT_NOTE), "got: {s}");

        let s = format!("{:#}", annotate_error(transient(), false));
        assert!(!s.contains("note:"), "got: {s}");

        let s = format!("{:#}", annotate_error(api(), true));
        assert!(!s.contains("note:"), "got: {s}");
    }
}
