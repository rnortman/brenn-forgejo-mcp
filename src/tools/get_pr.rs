//! `forgejo_get_pr` — GET `/api/v1/repos/{owner}/{repo}/pulls/{index}`.

use anyhow::{Result, anyhow};
use brenn_mcp::Tool;
use serde_json::{Value, json};

use crate::client::ForgejoClient;
use crate::tools::{pr_id_properties, required_str, required_u64};

pub struct ForgejoGetPrTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoGetPrTool {
    fn name(&self) -> &str {
        "forgejo_get_pr"
    }

    fn description(&self) -> &str {
        "Fetch one PR by number. Returns the full Forgejo PR object, including \
         title, body, state, merged, head {ref, sha, ...} and base {ref, sha, \
         ...}. An index that is an issue rather than a PR is a 404 API error. \
         To find a PR by its head branch name, use `forgejo_list_prs`."
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
        let pr = self.client.get_pull(owner, repo, index)?;
        if pr.is_null() {
            return Err(anyhow!(
                "Forgejo returned null for PR {owner}/{repo}#{index}: it could not render that PR (server-side failure; see Forgejo logs)"
            ));
        }
        Ok(pr)
    }
}
