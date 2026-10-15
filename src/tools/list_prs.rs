//! `forgejo_list_prs` — paginated GET `/api/v1/repos/{owner}/{repo}/pulls`
//! with an optional client-side filter on the head branch.
//!
//! The `/pulls` endpoint on the deployed Forgejo has no `head` query
//! parameter (an unknown one would be silently ignored), so the head
//! filter is applied here, against `head.label`. Forgejo sets `head.label`
//! to the head branch name whether or not the branch still exists, while
//! `head.ref` becomes `refs/pull/N/head` once the branch is deleted. Each
//! returned entry is a
//! subset of the Forgejo PR object with Forgejo's keys and nesting: the raw
//! object embeds two full `Repository` objects per PR, which is noise for
//! an LLM looking for `number` / `head.sha` / `base.ref`.

use anyhow::{Result, anyhow};
use brenn_mcp::Tool;
use log::warn;
use serde_json::{Map, Value, json};

use crate::client::ForgejoClient;
use crate::tools::{fetch_pages, optional_str, owner_repo_properties, required_str};

/// Per-page `limit` for `/pulls`.
const PULLS_PAGE_LIMIT: u32 = 50;

/// Cap on `/pulls` pagination: 10 pages; 500 PRs at Forgejo's default page size.
const PULLS_MAX_PAGES: u32 = 10;

/// Accepted `state` values; `open` is applied when `state` is absent.
const STATES: [&str; 3] = ["open", "closed", "all"];

/// Top-level PR keys copied into each entry when present.
const OPTIONAL_PR_KEYS: [&str; 10] = [
    "id",
    "title",
    "state",
    "merged",
    "merged_at",
    "draft",
    "html_url",
    "created_at",
    "updated_at",
    "merge_base",
];

/// Keys copied from `head` / `base` (`PRBranchInfo`) when present.
const BRANCH_KEYS: [&str; 4] = ["label", "ref", "sha", "repo_id"];

/// Keys copied from `user` when present.
const USER_KEYS: [&str; 2] = ["id", "login"];

pub struct ForgejoListPrsTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoListPrsTool {
    fn name(&self) -> &str {
        "forgejo_list_prs"
    }

    fn description(&self) -> &str {
        "List a repo's PRs, optionally only those whose head branch is `head`. \
         Use this to find a branch's PR again in a later session. `head` is a \
         bare branch name matched exactly against `head.label` (not \
         `owner:branch`); `head.label` keeps the branch name after the branch \
         is deleted, while `head.ref` then reads `refs/pull/N/head`. PRs \
         created by AGit-flow push (`git push … refs/for/<base>`) have an \
         empty `head.label` and are never returned by a `head` filter; list \
         without `head`, or use `forgejo_get_pr` by number, to find those. Use \
         `state: all` to include merged and closed PRs. Optional `state`: \
         open (default), closed, all. \
         Response shape: {pulls: [{id, number, title, state, merged, \
         merged_at, draft, html_url, created_at, updated_at, merge_base, \
         user: {id, login}, head: {label, ref, sha, repo_id}, base: {label, \
         ref, sha, repo_id}}], truncated?, truncated_detail?}. `head.sha` is \
         the PR's current head commit as Forgejo sees it (compare with local \
         `git rev-parse`); `base.ref` is the target branch. `body` is not \
         included; use `forgejo_get_pr` for the full PR object. At most 10 \
         pages are scanned (500 PRs at Forgejo's default page size of 50); if `truncated` is set, an empty or partial result \
         is not evidence that no matching PR exists."
    }

    fn input_schema(&self) -> Value {
        let mut props = owner_repo_properties();
        props.insert(
            "state".into(),
            json!({
                "type": "string",
                "enum": STATES,
                "description": "Optional: PR state to list. Default: open."
            }),
        );
        props.insert(
            "head".into(),
            json!({
                "type": "string",
                "description": "Optional: head branch name (not owner:branch); only PRs whose head.label equals it are returned."
            }),
        );
        json!({
            "type": "object",
            "properties": Value::Object(props),
            "required": ["owner", "repo"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let state = optional_str(&args, "state")?.unwrap_or("open");
        if !STATES.contains(&state) {
            return Err(anyhow!(
                "state must be one of {}; got {state:?}",
                STATES.join(", ")
            ));
        }
        let head = optional_str(&args, "head")?;
        if head == Some("") {
            return Err(anyhow!("head must not be empty"));
        }

        let (batch, page_cap_hit) = fetch_pages(PULLS_MAX_PAGES, |page| {
            let (prs, total) =
                self.client
                    .list_pulls(owner, repo, state, page, PULLS_PAGE_LIMIT)?;
            if let Some(i) = prs.iter().position(Value::is_null) {
                return Err(anyhow!(
                    "Forgejo returned null for the PR at page {page} position {} of {owner}/{repo} pulls (state={state}): it could not render that PR (server-side failure; see Forgejo logs)",
                    i + 1
                ));
            }
            Ok((prs, total))
        })?;
        if page_cap_hit {
            warn!(
                "list_prs: hit {PULLS_MAX_PAGES}-page cap on pulls for {owner}/{repo} (state={state})"
            );
        }
        let scanned = batch.len();
        let pulls: Vec<Value> = batch
            .iter()
            .map(|pr| project_pull(pr, owner, repo))
            .filter(|p| head.is_none_or(|h| p["head"]["label"].as_str() == Some(h)))
            .collect();

        let mut envelope = json!({ "pulls": pulls });
        if page_cap_hit {
            envelope["truncated"] = json!(true);
            envelope["truncated_detail"] = json!(build_truncated_detail(scanned, state, head));
        }
        Ok(envelope)
    }
}

/// Project one Forgejo PR object to the subset `forgejo_list_prs` returns,
/// keeping Forgejo's keys and nesting. `number`, `head`, `base` and a
/// string `head.label` are required; their absence is a protocol break.
fn project_pull(pr: &Value, owner: &str, repo: &str) -> Value {
    let obj = pr.as_object().unwrap_or_else(|| {
        panic!("protocol break: Forgejo /pulls entry is not an object (on {owner}/{repo})")
    });
    let number = obj
        .get("number")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            panic!("protocol break: Forgejo PR object has no numeric `number` (on {owner}/{repo})")
        });

    let mut out = Map::new();
    out.insert("number".into(), json!(number));
    for k in OPTIONAL_PR_KEYS {
        if let Some(v) = obj.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(user) = obj.get("user").and_then(|v| v.as_object()) {
        out.insert("user".into(), pick(user, &USER_KEYS));
    }
    for side in ["head", "base"] {
        let branch = obj
            .get(side)
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| {
                panic!("protocol break: Forgejo PR {owner}/{repo}#{number} has no `{side}` object")
            });
        out.insert(side.into(), pick(branch, &BRANCH_KEYS));
    }
    if !out["head"]["label"].is_string() {
        panic!("protocol break: Forgejo PR {owner}/{repo}#{number} has no string `head.label`");
    }
    Value::Object(out)
}

/// Copy the listed keys that are present in `src` into a new object.
fn pick(src: &Map<String, Value>, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for &k in keys {
        if let Some(v) = src.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    Value::Object(m)
}

/// `truncated_detail` for a scan that hit the page cap. With a head filter,
/// says explicitly that a matching PR may lie beyond the scanned pages.
fn build_truncated_detail(scanned: usize, state: &str, head: Option<&str>) -> String {
    let base = format!("hit {PULLS_MAX_PAGES}-page cap ({scanned} PRs scanned, state={state})");
    match head {
        None => format!("{base}; older PRs were not listed"),
        Some(h) => format!("{base}; a PR with head `{h}` may exist beyond the scanned pages"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_pr() -> Value {
        json!({
            "id": 900,
            "number": 7,
            "title": "T",
            "body": "long description",
            "state": "open",
            "merged": false,
            "merged_at": null,
            "draft": false,
            "html_url": "https://example.com/a/r/pulls/7",
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": "2026-10-02T00:00:00Z",
            "merge_base": "abc",
            "comments": 3,
            "user": { "id": 1, "login": "alice", "email": "a@example.com" },
            "head": {
                "label": "feature", "ref": "feature", "sha": "h1", "repo_id": 5,
                "repo": { "id": 5, "name": "r" }
            },
            "base": {
                "label": "main", "ref": "main", "sha": "b1", "repo_id": 5,
                "repo": { "id": 5, "name": "r" }
            }
        })
    }

    #[test]
    fn projection_keeps_listed_keys_only() {
        let p = project_pull(&full_pr(), "a", "r");
        assert_eq!(
            p,
            json!({
                "id": 900,
                "number": 7,
                "title": "T",
                "state": "open",
                "merged": false,
                "merged_at": null,
                "draft": false,
                "html_url": "https://example.com/a/r/pulls/7",
                "created_at": "2026-10-01T00:00:00Z",
                "updated_at": "2026-10-02T00:00:00Z",
                "merge_base": "abc",
                "user": { "id": 1, "login": "alice" },
                "head": { "label": "feature", "ref": "feature", "sha": "h1", "repo_id": 5 },
                "base": { "label": "main", "ref": "main", "sha": "b1", "repo_id": 5 }
            })
        );
    }

    #[test]
    fn projection_omits_absent_optional_keys() {
        let pr = json!({
            "number": 3,
            "head": { "label": "f" },
            "base": { "ref": "main" }
        });
        let p = project_pull(&pr, "a", "r");
        assert_eq!(
            p,
            json!({ "number": 3, "head": { "label": "f" }, "base": { "ref": "main" } })
        );
    }

    #[test]
    #[should_panic(expected = "has no `base` object")]
    fn projection_panics_without_base() {
        project_pull(&json!({ "number": 3, "head": { "label": "f" } }), "a", "r");
    }

    #[test]
    #[should_panic(expected = "no numeric `number`")]
    fn projection_panics_without_number() {
        project_pull(
            &json!({ "head": { "label": "f" }, "base": { "ref": "main" } }),
            "a",
            "r",
        );
    }

    #[test]
    #[should_panic(expected = "no string `head.label`")]
    fn projection_panics_on_non_string_head_label() {
        project_pull(
            &json!({ "number": 3, "head": { "label": 4 }, "base": { "ref": "main" } }),
            "a",
            "r",
        );
    }

    #[test]
    fn detail_without_head() {
        let d = build_truncated_detail(500, "open", None);
        assert!(d.contains("10-page cap"), "{d}");
        assert!(d.contains("500 PRs scanned"), "{d}");
        assert!(d.contains("state=open"), "{d}");
        assert!(!d.contains("may exist"), "{d}");
    }

    #[test]
    fn detail_with_head_says_match_may_exist_beyond_scan() {
        let d = build_truncated_detail(200, "all", Some("feature"));
        assert!(d.contains("10-page cap"), "{d}");
        assert!(d.contains("200 PRs scanned"), "{d}");
        assert!(
            d.contains("a PR with head `feature` may exist beyond the scanned pages"),
            "{d}"
        );
    }
}
