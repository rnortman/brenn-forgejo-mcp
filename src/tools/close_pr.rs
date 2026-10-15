//! `forgejo_close_pr` — PATCH `/api/v1/repos/{owner}/{repo}/pulls/{index}`
//! with `{"state": "closed"}`.

use anyhow::Result;
use brenn_mcp::Tool;
use serde_json::{Value, json};

use crate::client::ForgejoClient;
use crate::tools::{pr_id_properties, required_str, required_u64};

pub struct ForgejoClosePrTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoClosePrTool {
    fn name(&self) -> &str {
        "forgejo_close_pr"
    }

    fn description(&self) -> &str {
        "Close a PR (not merge). PATCHes the PR to state=closed. \
         Returns the updated PR object so the LLM can confirm the final state. \
         Closing an already-closed PR is a no-op: returns state=closed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": Value::Object(pr_id_properties()),
            "required": ["owner", "repo", "index"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let index = required_u64(&args, "index")?;
        self.client
            .patch_pull(owner, repo, index, &json!({ "state": "closed" }))
    }
}
