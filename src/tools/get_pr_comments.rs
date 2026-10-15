//! `forgejo_get_pr_comments` — GET
//! `/api/v1/repos/{owner}/{repo}/issues/{index}/comments`.
//!
//! Returns the PR's plain conversation comments (not inline review
//! comments or review bodies). The endpoint is unpaginated, so this
//! assumes one response is the whole thread and reports no truncation.

use anyhow::Result;
use brenn_mcp::Tool;
use serde_json::{Value, json};

use crate::client::ForgejoClient;
use crate::tools::{optional_str, pr_id_properties, required_str, required_u64};

pub struct ForgejoGetPrCommentsTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoGetPrCommentsTool {
    fn name(&self) -> &str {
        "forgejo_get_pr_comments"
    }

    fn description(&self) -> &str {
        "Return a PR's conversation thread: the comments typed into the PR's \
         comment box. These are distinct from review bodies and inline \
         comments, which `forgejo_get_review_comments` returns; read both to \
         see all feedback on a PR. `index` is a PR number. Optional `since` \
         (RFC 3339 timestamp, passed to Forgejo verbatim) limits the result \
         to comments updated at or after that time. Response shape: \
         {comments: [...Forgejo Comment objects: id, body, user, html_url, \
         created_at, updated_at, ...]}. The whole thread is returned in one \
         response."
    }

    fn input_schema(&self) -> Value {
        let mut props = pr_id_properties();
        props.insert(
            "since".into(),
            json!({
                "type": "string",
                "description": "Optional RFC 3339 timestamp: only comments updated at or after this time."
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
        let since = optional_str(&args, "since")?;
        let comments = self.client.list_issue_comments(owner, repo, index, since)?;
        Ok(json!({ "comments": comments }))
    }
}
