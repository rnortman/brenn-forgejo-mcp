//! `forgejo_get_review_comments` — return all reviews on a PR together with
//! their inline comments.
//!
//! Two endpoints:
//! - GET `/pulls/{index}/reviews` (paginated)
//! - GET `/pulls/{index}/reviews/{id}/comments` (NOT paginated per swagger;
//!   we detect `len == MAX_RESPONSE_ITEMS` as a conservative truncation
//!   signal)

use anyhow::Result;
use brenn_mcp::Tool;
use log::warn;
use serde_json::{Value, json};

use crate::client::ForgejoClient;
use crate::tools::{pr_id_properties, required_str, required_u64};

/// Forgejo/Gitea `MAX_RESPONSE_ITEMS` default. The
/// `/reviews/{id}/comments` endpoint is not paginated, so we treat
/// `len == MAX_RESPONSE_ITEMS` as a conservative truncation signal — we
/// can't distinguish "exactly that many existed" from "server clamped us".
const FORGEJO_MAX_RESPONSE_ITEMS: usize = 50;

/// Per-page `limit` for the paginated `/reviews` endpoint.
const REVIEWS_PAGE_LIMIT: u32 = 50;

/// Cap on `/reviews` pagination. Worst case: 10 × 50 = 500 reviews.
const REVIEWS_MAX_PAGES: u32 = 10;

pub struct ForgejoGetReviewCommentsTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoGetReviewCommentsTool {
    fn name(&self) -> &str {
        "forgejo_get_review_comments"
    }

    fn description(&self) -> &str {
        "Return every review on a PR together with its inline comments. \
         Optional `state` filters to reviews in that state (APPROVED, \
         CHANGES_REQUESTED, COMMENT, PENDING, DISMISSED). Response shape: \
         {reviews: [{id, state, body, user, submitted_at, comments: [...]}], \
         truncated?, truncated_detail?}. Inline comment line fields are \
         `position` (new side) / `original_position` (old side); note this \
         differs from the write-side field names (`new_position` / \
         `old_position`) in `forgejo_post_review`."
    }

    fn input_schema(&self) -> Value {
        let mut props = pr_id_properties();
        props.insert(
            "state".into(),
            json!({
                "type": "string",
                "enum": ["APPROVED", "CHANGES_REQUESTED", "COMMENT", "PENDING", "DISMISSED"],
                "description": "Optional: filter to reviews in this state."
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
        let state_filter = args.get("state").and_then(|v| v.as_str());

        let mut reviews: Vec<Value> = Vec::new();
        let mut reviews_page_cap_hit = false;
        let mut capped_review_ids: Vec<u64> = Vec::new();

        for page in 1..=REVIEWS_MAX_PAGES {
            let batch = self
                .client
                .list_reviews(owner, repo, index, page, REVIEWS_PAGE_LIMIT)?;
            let got = batch.len();
            reviews.extend(batch);
            if got < REVIEWS_PAGE_LIMIT as usize {
                break;
            }
            if page == REVIEWS_MAX_PAGES && got == REVIEWS_PAGE_LIMIT as usize {
                reviews_page_cap_hit = true;
                warn!(
                    "get_review_comments: hit {REVIEWS_MAX_PAGES}-page cap on reviews for {owner}/{repo}#{index}"
                );
            }
        }

        let mut enriched: Vec<Value> = Vec::with_capacity(reviews.len());
        for review in reviews {
            if let Some(s) = state_filter {
                let rstate = review.get("state").and_then(|v| v.as_str()).unwrap_or("");
                if rstate != s {
                    continue;
                }
            }
            let review_id = review.get("id").and_then(|v| v.as_u64()).unwrap_or_else(|| {
                panic!(
                    "protocol break: Forgejo review object has no numeric `id` (on {owner}/{repo}#{index})"
                )
            });
            let comments = self
                .client
                .list_review_comments(owner, repo, index, review_id)?;
            if comments.len() == FORGEJO_MAX_RESPONSE_ITEMS {
                capped_review_ids.push(review_id);
                warn!(
                    "get_review_comments: review {review_id} on {owner}/{repo}#{index} hit MAX_RESPONSE_ITEMS cap ({FORGEJO_MAX_RESPONSE_ITEMS})"
                );
            }
            let mut enriched_review = match review {
                Value::Object(m) => Value::Object(m),
                other => {
                    panic!("protocol break: Forgejo /reviews entry is not an object: {other:?}")
                }
            };
            if let Value::Object(ref mut map) = enriched_review {
                map.insert("comments".to_string(), Value::Array(comments));
            }
            enriched.push(enriched_review);
        }

        let mut envelope = json!({ "reviews": enriched });
        let detail = build_truncated_detail(reviews_page_cap_hit, &capped_review_ids, index);
        if let Some(d) = detail {
            envelope["truncated"] = json!(true);
            envelope["truncated_detail"] = json!(d);
        }
        Ok(envelope)
    }
}

/// Build the envelope's `truncated_detail` string. Returns `None` when no
/// truncation condition tripped. When both conditions tripped, both are
/// reported in a single message so neither gets swallowed.
fn build_truncated_detail(
    reviews_page_cap_hit: bool,
    capped_review_ids: &[u64],
    index: u64,
) -> Option<String> {
    match (reviews_page_cap_hit, capped_review_ids.is_empty()) {
        (false, true) => None,
        (true, true) => Some(format!(
            "hit {REVIEWS_MAX_PAGES}-page cap on reviews for PR {index}"
        )),
        (false, false) => Some(format!(
            "reviews {capped_review_ids:?} each had >= MAX_RESPONSE_ITEMS comments; tail not fetched (endpoint does not paginate)"
        )),
        (true, false) => Some(format!(
            "hit {REVIEWS_MAX_PAGES}-page cap on reviews for PR {index}; also reviews {capped_review_ids:?} each had >= MAX_RESPONSE_ITEMS comments (tail not fetched)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_none_when_nothing_capped() {
        assert!(build_truncated_detail(false, &[], 5).is_none());
    }

    #[test]
    fn detail_lists_all_capped_review_ids() {
        let d = build_truncated_detail(false, &[10, 42, 99], 5).unwrap();
        assert!(d.contains("10"), "{d}");
        assert!(d.contains("42"), "{d}");
        assert!(d.contains("99"), "{d}");
    }

    #[test]
    fn detail_combines_page_cap_and_comment_caps() {
        let d = build_truncated_detail(true, &[10], 5).unwrap();
        assert!(d.contains("10-page cap"), "{d}");
        assert!(d.contains("10"), "{d}"); // both the page cap constant 10 and the review id 10 present
        assert!(d.contains("PR 5"), "{d}");
    }
}
