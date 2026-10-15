//! The Forgejo PR tools, plus shared schema helpers.

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

pub mod add_review_comment;
pub mod close_pr;
pub mod get_pr;
pub mod get_pr_comments;
pub mod list_prs;
pub mod list_review_comments;
pub mod open_pr;
pub mod post_review;
pub mod update_pr;

pub use add_review_comment::ForgejoAddReviewCommentTool;
pub use close_pr::ForgejoClosePrTool;
pub use get_pr::ForgejoGetPrTool;
pub use get_pr_comments::ForgejoGetPrCommentsTool;
pub use list_prs::ForgejoListPrsTool;
pub use list_review_comments::ForgejoGetReviewCommentsTool;
pub use open_pr::ForgejoOpenPrTool;
pub use post_review::ForgejoPostReviewTool;
pub use update_pr::ForgejoUpdatePrTool;

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

/// `{owner, repo, index}` property block shared by every tool that targets
/// an existing PR.
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

/// Parse an optional string field from args. Absent or `null` is
/// `Ok(None)`; a present non-string value is an error rather than being
/// silently treated as absent.
pub(crate) fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(anyhow!("field {name} must be a string")),
    }
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

/// Fetch pages `1..=max_pages` of a paged Forgejo endpoint. Each `fetch`
/// returns a page and the endpoint's `X-Total-Count` — the number of rows
/// the server paginates over. The scan is complete after page `k` when
/// `k * max(widest, 1) >= total`, where `widest` is the longest page seen:
/// the server's page size is at least `widest` and at least 1, so every
/// row fits in the pages fetched. Neither a short page nor an empty page
/// is evidence of the end: Forgejo clamps `limit` to its `[api]
/// MAX_RESPONSE_ITEMS`, and `/reviews` hides other users' pending reviews
/// *after* cutting the page, so a page can be `[]` before the end. (The
/// floor is what lets a run of empty pages — every row a hidden draft —
/// satisfy the bound instead of running to the cap.) Returns every item
/// fetched and whether the cap was reached before completeness was
/// proven, i.e. more items may exist beyond the cap. Exact when any page
/// was unfiltered; conservative when every page seen was partially
/// filtered; never short.
pub(crate) fn fetch_pages(
    max_pages: u32,
    mut fetch: impl FnMut(u32) -> Result<(Vec<Value>, u64)>,
) -> Result<(Vec<Value>, bool)> {
    let mut items = Vec::new();
    let mut widest = 0usize;
    for page in 1..=max_pages {
        // `total` is re-read on every page; the latest value is compared.
        let (batch, total) = fetch(page)?;
        widest = widest.max(batch.len());
        items.extend(batch);
        if u64::from(page) * (widest.max(1) as u64) >= total {
            return Ok((items, false));
        }
    }
    Ok((items, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages_of(
        sizes: &'static [usize],
        total: u64,
    ) -> impl FnMut(u32) -> Result<(Vec<Value>, u64)> {
        move |page| Ok((vec![json!(page); sizes[page as usize - 1]], total))
    }

    /// Run `fetch_pages` over `pages_of(sizes, total)`, recording the pages
    /// requested.
    fn scan(max_pages: u32, sizes: &'static [usize], total: u64) -> (Vec<Value>, bool, Vec<u32>) {
        let mut requested = Vec::new();
        let mut fetch = pages_of(sizes, total);
        let (items, cap) = fetch_pages(max_pages, |page| {
            requested.push(page);
            fetch(page)
        })
        .unwrap();
        (items, cap, requested)
    }

    #[test]
    fn fetch_pages_stops_when_widest_covers_total() {
        let (items, cap, requested) = scan(10, &[3], 3);
        assert_eq!(items, vec![json!(1); 3]);
        assert!(!cap);
        assert_eq!(requested, vec![1]);
    }

    #[test]
    fn fetch_pages_zero_total_is_one_fetch() {
        let (items, cap, requested) = scan(10, &[0], 0);
        assert!(items.is_empty());
        assert!(!cap);
        assert_eq!(requested, vec![1]);
    }

    #[test]
    fn fetch_pages_single_hidden_row_is_one_fetch() {
        let (items, cap, requested) = scan(10, &[0], 1);
        assert!(items.is_empty());
        assert!(!cap);
        assert_eq!(requested, vec![1]);
    }

    #[test]
    fn fetch_pages_all_hidden_stops_after_total_pages() {
        let (items, cap, requested) = scan(10, &[0, 0, 0], 3);
        assert!(items.is_empty());
        assert!(!cap);
        assert_eq!(requested, vec![1, 2, 3]);
    }

    #[test]
    fn fetch_pages_all_hidden_past_cap_reports_cap() {
        let (items, cap, requested) = scan(2, &[0, 0], 3);
        assert!(items.is_empty());
        assert!(cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_exact_multiple_needs_no_extra_request() {
        let (items, cap, requested) = scan(10, &[2, 2], 4);
        assert_eq!(items, vec![json!(1), json!(1), json!(2), json!(2)]);
        assert!(!cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_exact_multiple_at_cap_is_not_flagged() {
        let (items, cap, requested) = scan(2, &[2, 2], 4);
        assert_eq!(items.len(), 4);
        assert!(!cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_continues_past_empty_page() {
        let (items, cap, requested) = scan(10, &[0, 2], 4);
        assert_eq!(items, vec![json!(2), json!(2)]);
        assert!(!cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_short_pages_under_clamp() {
        let (items, cap, requested) = scan(10, &[20, 20, 5], 45);
        assert_eq!(items.len(), 45);
        assert!(!cap);
        assert_eq!(requested, vec![1, 2, 3]);
    }

    #[test]
    fn fetch_pages_filtered_page_keeps_widest_bound() {
        let (items, cap, requested) = scan(10, &[5, 2], 10);
        assert_eq!(items.len(), 7);
        assert!(!cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_reports_cap_when_total_not_covered() {
        let (items, cap, requested) = scan(2, &[1, 1], 5);
        assert_eq!(items.len(), 2);
        assert!(cap);
        assert_eq!(requested, vec![1, 2]);
    }

    #[test]
    fn fetch_pages_propagates_error() {
        let err = fetch_pages(3, |_| Err(anyhow!("boom"))).unwrap_err();
        assert_eq!(format!("{err:#}"), "boom");
    }

    #[test]
    fn optional_str_absent_is_none() {
        assert_eq!(optional_str(&json!({}), "x").unwrap(), None);
    }

    #[test]
    fn optional_str_null_is_none() {
        assert_eq!(optional_str(&json!({ "x": null }), "x").unwrap(), None);
    }

    #[test]
    fn optional_str_string_is_some() {
        assert_eq!(optional_str(&json!({ "x": "v" }), "x").unwrap(), Some("v"));
        assert_eq!(optional_str(&json!({ "x": "" }), "x").unwrap(), Some(""));
    }

    #[test]
    fn optional_str_non_string_errors() {
        let err = optional_str(&json!({ "x": 42 }), "x").unwrap_err();
        assert_eq!(format!("{err:#}"), "field x must be a string");
    }

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
