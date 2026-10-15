//! `forgejo_add_review_comment` — POST
//! `/api/v1/repos/{owner}/{repo}/pulls/{index}/reviews/{review_id}/comments`.
//!
//! Appends a single inline comment to an existing review. Forgejo's REST
//! API does NOT accept a `reply_to` / `in_reply_to` field — there is no
//! threaded-reply primitive. The UI groups comments on the same
//! `path` + `new_position` / `old_position` into one conversation, so
//! posting at the same location as a reviewer comment reads as a reply.

use anyhow::Result;
use brenn_mcp::Tool;
use serde_json::{Value, json};

use crate::client::ForgejoClient;
use crate::tools::{
    inline_comment_properties, pr_id_properties, required_str, required_u64,
    validate_inline_comment,
};

pub struct ForgejoAddReviewCommentTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoAddReviewCommentTool {
    fn name(&self) -> &str {
        "forgejo_add_review_comment"
    }

    fn description(&self) -> &str {
        "Append an inline comment to an existing PR review. Requires `review_id` \
         (the review to attach to, obtained via `forgejo_get_review_comments`), \
         `path`, `body`, and exactly one of `new_position` (post-change side) or \
         `old_position` (pre-change side). Field names match `forgejo_post_review`'s \
         inline `comments[]` shape. Note: Forgejo's REST API does NOT support \
         threading a reply under a specific parent comment — this tool adds a new \
         comment to a review, not a threaded reply. Forgejo's UI groups comments \
         on the same file+line into a single conversation, so posting at the same \
         path+line as an earlier comment reads as a reply."
    }

    fn input_schema(&self) -> Value {
        let mut props = pr_id_properties();
        props.insert(
            "review_id".into(),
            json!({
                "type": "integer",
                "description": "ID of the existing review to attach to."
            }),
        );
        // path / body / new_position / old_position come from the shared
        // helper so this tool's schema stays in lockstep with the
        // `comments[].items` schema in `forgejo_post_review` and with the
        // runtime validator in `validate_inline_comment`.
        props.extend(inline_comment_properties());
        json!({
            "type": "object",
            "properties": Value::Object(props),
            "required": ["owner", "repo", "index", "review_id", "path", "body"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let index = required_u64(&args, "index")?;
        let review_id = required_u64(&args, "review_id")?;

        // Reuse the shared validator so schema and error text match the
        // inline-comments array in `forgejo_post_review` exactly. `None`
        // → top-level call, errors have no `comments[i]: ` prefix.
        let payload = validate_inline_comment(None, &args)?;

        self.client
            .create_review_comment(owner, repo, index, review_id, &payload)
    }
}
