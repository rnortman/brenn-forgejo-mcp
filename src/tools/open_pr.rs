//! `forgejo_open_pr` — POST `/api/v1/repos/{owner}/{repo}/pulls`.

use anyhow::Result;
use brenn_mcp::Tool;
use serde_json::{Map, Value, json};

use crate::client::ForgejoClient;
use crate::tools::{optional_str, owner_repo_properties, required_str};

pub struct ForgejoOpenPrTool {
    pub client: ForgejoClient,
}

impl Tool for ForgejoOpenPrTool {
    fn name(&self) -> &str {
        "forgejo_open_pr"
    }

    fn description(&self) -> &str {
        "Open a pull request on Forgejo. Required: owner, repo, title, head, base. \
         Optional: body (markdown). Returns the full Forgejo PR object \
         (includes number, html_url, state)."
    }

    fn input_schema(&self) -> Value {
        let mut props = owner_repo_properties();
        props.insert(
            "title".into(),
            json!({ "type": "string", "description": "PR title." }),
        );
        props.insert(
            "body".into(),
            json!({ "type": "string", "description": "Markdown PR description. Optional." }),
        );
        props.insert(
            "head".into(),
            json!({
                "type": "string",
                "description": "Source branch (or fork:branch for cross-repo PRs)."
            }),
        );
        props.insert(
            "base".into(),
            json!({ "type": "string", "description": "Target branch, e.g. main." }),
        );
        json!({
            "type": "object",
            "properties": Value::Object(props),
            "required": ["owner", "repo", "title", "head", "base"]
        })
    }

    fn call(&self, args: Value) -> Result<Value> {
        let owner = required_str(&args, "owner")?;
        let repo = required_str(&args, "repo")?;
        let title = required_str(&args, "title")?;
        let head = required_str(&args, "head")?;
        let base = required_str(&args, "base")?;

        let mut body_obj = Map::new();
        body_obj.insert("title".to_string(), json!(title));
        body_obj.insert("head".to_string(), json!(head));
        body_obj.insert("base".to_string(), json!(base));
        if let Some(body) = optional_str(&args, "body")? {
            body_obj.insert("body".to_string(), json!(body));
        }

        self.client
            .create_pull(owner, repo, &Value::Object(body_obj))
    }
}
