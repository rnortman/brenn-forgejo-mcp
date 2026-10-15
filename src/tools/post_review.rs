//! `forgejo_post_review` — POST `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews`.
//!
//! We require exactly one of `new_position` / `old_position` per inline
//! comment. Forgejo's swagger permits both as optional; we reject both-set
//! and neither-set client-side as ambiguous.

use anyhow::{Result, anyhow};
use brenn_mcp::Tool;
use serde_json::{Map, Value, json};

use crate::client::ForgejoClient;
use crate::tools::{
    inline_comment_properties, pr_id_properties, required_str, required_u64,
    validate_inline_comment,
};

pub struct ForgejoPostReviewTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoPostReviewTool {
    fn name(&self) -> &str {
        "forgejo_post_review"
    }

    fn description(&self) -> &str {
        "Post a review on a PR: APPROVE, REQUEST_CHANGES, COMMENT, or PENDING. \
         Optional `body` summary and optional array of inline `comments` (each \
         with `path`, `body`, and exactly one of `new_position` (post-change \
         side) or `old_position` (pre-change side)). Note: the field names \
         here differ from the read side (`forgejo_get_review_comments`), \
         where the fields are `position` / `original_position` — this \
         reflects Forgejo's own API and we don't translate."
    }

    fn input_schema(&self) -> Value {
        let mut props = pr_id_properties();
        props.insert(
            "event".into(),
            json!({
                "type": "string",
                "enum": ["APPROVE", "REQUEST_CHANGES", "COMMENT", "PENDING"],
                "description": "Review event type."
            }),
        );
        props.insert(
            "body".into(),
            json!({
                "type": "string",
                "description": "Summary body. Required unless event is APPROVE (Forgejo enforces server-side)."
            }),
        );
        props.insert(
            "comments".into(),
            json!({
                "type": "array",
                "description": "Inline comments on specific lines.",
                "items": {
                    "type": "object",
                    "properties": Value::Object(inline_comment_properties()),
                    "required": ["path", "body"]
                }
            }),
        );
        json!({
            "type": "object",
            "properties": Value::Object(props),
            "required": ["owner", "repo", "index", "event"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let index = required_u64(&args, "index")?;
        let event = required_str(&args, "event")?;

        let mut body_obj = Map::new();
        body_obj.insert("event".to_string(), json!(event));
        if let Some(body) = args.get("body").and_then(|v| v.as_str()) {
            body_obj.insert("body".to_string(), json!(body));
        }

        if let Some(comments_val) = args.get("comments") {
            let comments_arr = comments_val
                .as_array()
                .ok_or_else(|| anyhow!("`comments` must be an array"))?;
            let validated: Vec<Value> = comments_arr
                .iter()
                .enumerate()
                .map(|(i, c)| validate_inline_comment(Some(i), c))
                .collect::<Result<_>>()?;
            body_obj.insert("comments".to_string(), Value::Array(validated));
        }

        self.client
            .create_review(owner, repo, index, &Value::Object(body_obj))
    }
}
