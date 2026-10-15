//! The five Forgejo PR tools, plus shared schema helpers.

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

pub mod add_review_comment;
pub mod close_pr;
pub mod list_review_comments;
pub mod open_pr;
pub mod post_review;

pub use add_review_comment::ForgejoAddReviewCommentTool;
pub use close_pr::ForgejoClosePrTool;
pub use list_review_comments::ForgejoGetReviewCommentsTool;
pub use open_pr::ForgejoOpenPrTool;
pub use post_review::ForgejoPostReviewTool;

/// `{owner, repo}` property block shared by every PR-touching tool.
pub fn owner_repo_properties() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "owner".into(),
        json!({ "type": "string", "description": "Repository owner (user or org slug)." }),
    );
    m.insert(
        "repo".into(),
        json!({ "type": "string", "description": "Repository name." }),
    );
    m
}

/// `{owner, repo, index}` property block shared by the three tools that
/// target an existing PR.
pub fn pr_id_properties() -> Map<String, Value> {
    let mut m = owner_repo_properties();
    m.insert(
        "index".into(),
        json!({ "type": "integer", "description": "PR number within the repo." }),
    );
    m
}

/// The four inline-comment fields (`path`, `body`, `new_position`,
/// `old_position`) shared by `forgejo_post_review`'s `comments[].items`
/// schema and `forgejo_add_review_comment`'s top-level schema. Keeping this
/// in one place guarantees the two tool schemas stay in lockstep with each
/// other and with `validate_inline_comment`'s runtime behaviour.
pub fn inline_comment_properties() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "path".into(),
        json!({ "type": "string", "description": "Path to file in the repo." }),
    );
    m.insert(
        "body".into(),
        json!({ "type": "string", "description": "Markdown comment body." }),
    );
    m.insert(
        "new_position".into(),
        json!({
            "type": "integer",
            "description": "Line on the new (post-change) side. Use this OR old_position, not both."
        }),
    );
    m.insert(
        "old_position".into(),
        json!({
            "type": "integer",
            "description": "Line on the old (pre-change) side."
        }),
    );
    m
}

/// Parse a required string field from args, returning the framework's
/// `anyhow::Error` shape ("missing required field: <name>").
pub fn required_str<'a>(args: &'a Value, name: &str) -> anyhow::Result<&'a str> {
    args.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: {name}"))
}

/// Parse a required u64 field from args.
pub fn required_u64(args: &Value, name: &str) -> anyhow::Result<u64> {
    args.get(name)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid field: {name}"))
}

/// Validate a single inline-comment object and return the payload to send.
/// We pass `path`, `body`, and exactly one of `new_position` / `old_position`
/// through verbatim. Shared between `forgejo_post_review`'s `comments[]`
/// entries and `forgejo_add_review_comment`'s top-level payload.
///
/// `idx` is the comment's zero-based index within a `comments[]` array, used
/// as `"comments[{idx}]: "` error prefix. `None` means this is a top-level
/// call (no prefix). Taking `Option<usize>` rather than a preformatted string
/// avoids a per-comment `String` allocation on the happy path: the label is
/// formatted into the error message only inside `anyhow!(...)`, which runs
/// only on the error branch.
pub(crate) fn validate_inline_comment(idx: Option<usize>, c: &Value) -> Result<Value> {
    // Helper so the two separator arms stay in sync. `Display` impl formats
    // `"comments[{i}]: "` or `""` without heap-allocating unless anyhow
    // actually renders the error.
    struct Prefix(Option<usize>);
    impl std::fmt::Display for Prefix {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                Some(i) => write!(f, "comments[{i}]: "),
                None => Ok(()),
            }
        }
    }
    let pre = Prefix(idx);

    let obj = c
        .as_object()
        .ok_or_else(|| anyhow!("{pre}must be an object"))?;

    let path = obj
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{pre}missing required field: path"))?;
    let body = obj
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{pre}missing required field: body"))?;

    let new_position = obj.get("new_position").filter(|&v| !v.is_null());
    let old_position = obj.get("old_position").filter(|&v| !v.is_null());

    match (new_position, old_position) {
        (Some(_), Some(_)) => Err(anyhow!(
            "{pre}exactly one of `new_position` or `old_position` is required; both are set"
        )),
        (None, None) => Err(anyhow!(
            "{pre}exactly one of `new_position` or `old_position` is required; neither is set"
        )),
        (Some(np), None) => {
            let n = np
                .as_u64()
                .ok_or_else(|| anyhow!("{pre}new_position must be a non-negative integer"))?;
            Ok(json!({ "path": path, "body": body, "new_position": n }))
        }
        (None, Some(op)) => {
            let n = op
                .as_u64()
                .ok_or_else(|| anyhow!("{pre}old_position must be a non-negative integer"))?;
            Ok(json!({ "path": path, "body": body, "old_position": n }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_both_positions_set() {
        let c = json!({ "path": "f", "body": "b", "new_position": 1, "old_position": 2 });
        let err = validate_inline_comment(Some(0), &c).unwrap_err();
        assert!(format!("{err:#}").contains("both are set"));
    }

    #[test]
    fn rejects_neither_position_set() {
        let c = json!({ "path": "f", "body": "b" });
        let err = validate_inline_comment(Some(0), &c).unwrap_err();
        assert!(format!("{err:#}").contains("neither is set"));
    }

    #[test]
    fn accepts_new_position_only() {
        let c = json!({ "path": "f", "body": "b", "new_position": 7 });
        let got = validate_inline_comment(Some(0), &c).unwrap();
        assert_eq!(got["new_position"], 7);
        assert!(got.get("old_position").is_none());
    }

    #[test]
    fn accepts_old_position_only() {
        let c = json!({ "path": "f", "body": "b", "old_position": 3 });
        let got = validate_inline_comment(Some(0), &c).unwrap();
        assert_eq!(got["old_position"], 3);
        assert!(got.get("new_position").is_none());
    }

    #[test]
    fn null_position_treated_as_absent() {
        // new_position: null + old_position: 3 → old-only, accepted.
        let c = json!({ "path": "f", "body": "b", "new_position": null, "old_position": 3 });
        let got = validate_inline_comment(Some(0), &c).unwrap();
        assert_eq!(got["old_position"], 3);
    }

    #[test]
    fn missing_path_errors() {
        let c = json!({ "body": "b", "new_position": 1 });
        let err = validate_inline_comment(Some(0), &c).unwrap_err();
        assert!(format!("{err:#}").contains("path"));
    }

    #[test]
    fn missing_body_errors() {
        let c = json!({ "path": "f", "new_position": 1 });
        let err = validate_inline_comment(Some(0), &c).unwrap_err();
        assert!(format!("{err:#}").contains("body"));
    }

    #[test]
    fn idx_some_prefixes_error() {
        let c = json!({ "path": "f", "body": "b" });
        let err = validate_inline_comment(Some(2), &c).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.starts_with("comments[2]: "), "got: {s}");
    }

    #[test]
    fn idx_none_omits_prefix() {
        let c = json!({ "path": "f", "body": "b" });
        let err = validate_inline_comment(None, &c).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.starts_with("exactly one of"),
            "expected unprefixed error, got: {s}"
        );
    }
}
