//! MCP server assembly: the server instructions and the registered tools.

use brenn_mcp::Server;

use crate::client::ForgejoClient;
use crate::tools::{
    ForgejoAddReviewCommentTool, ForgejoClosePrTool, ForgejoGetPrCommentsTool, ForgejoGetPrTool,
    ForgejoGetReviewCommentsTool, ForgejoListPrsTool, ForgejoOpenPrTool, ForgejoPostReviewTool,
    ForgejoUpdatePrTool,
};

/// Server-level instructions sent to the MCP client on `initialize`.
pub const INSTRUCTIONS: &str = "\
This server exposes Forgejo pull-request operations: open a PR, list and look up PRs, read existing reviews and inline comments, read a PR's conversation thread, post a review (approve / request changes / comment, optionally with inline comments), append an inline comment to a review, edit a PR's title / body / base, and close a PR. PR numbers are per-repo (\"index\" arg).

Feedback on a PR lives in two places: `forgejo_get_review_comments` returns reviews and their inline comments, `forgejo_get_pr_comments` returns the conversation thread (comments typed into the PR's comment box). Read both to see all feedback.

To find a PR when only its branch name is known, use `forgejo_list_prs` with `head` (the head branch name, matched against `head.label`, which keeps the branch name after the branch is deleted; `head.ref` then reads `refs/pull/N/head`) and `state: all` to include merged and closed PRs; each entry carries `number`, `head.sha` (the PR's current head commit) and `base.ref` (its target branch). `forgejo_get_pr` returns the full PR object, including `body`.

`forgejo_update_pr` edits a PR's title, body and base. `base` must go in its own call, without `title` or `body`: Forgejo applies title and body before the base, without a transaction. Closing a PR stays with `forgejo_close_pr`.

Inline-comment field naming is NOT symmetric between read and write (this reflects Forgejo's own API, we don't translate):
- When you READ comments via `forgejo_get_review_comments`, each comment has `position` (line on the post-change side) and `original_position` (line on the pre-change side).
- When you WRITE comments via `forgejo_post_review`, use `new_position` (post-change) and `old_position` (pre-change), and provide exactly one of the two per comment.

Errors returned as `isError: true` are safe to show the user. Transient errors (prefix \"Forgejo transient error:\") may be retried; API errors (prefix \"Forgejo API error (XXX):\") indicate an input or auth problem and should not be retried without changing something.

`forgejo_add_review_comment` appends an inline comment to an existing review (identified by `review_id`). Forgejo's REST API does not thread replies under a specific parent comment, but the UI groups comments on the same file+line into one conversation, so posting at the same position as a reviewer's comment reads as a reply.\
";

/// Build the MCP server with the instructions and every Forgejo PR tool,
/// each holding a clone of `client`.
pub fn build_server(client: ForgejoClient) -> Server {
    Server::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        .with_instructions(INSTRUCTIONS)
        .with_tool(ForgejoOpenPrTool {
            client: client.clone(),
        })
        .with_tool(ForgejoGetReviewCommentsTool {
            client: client.clone(),
        })
        .with_tool(ForgejoPostReviewTool {
            client: client.clone(),
        })
        .with_tool(ForgejoAddReviewCommentTool {
            client: client.clone(),
        })
        .with_tool(ForgejoClosePrTool {
            client: client.clone(),
        })
        .with_tool(ForgejoGetPrCommentsTool {
            client: client.clone(),
        })
        .with_tool(ForgejoListPrsTool {
            client: client.clone(),
        })
        .with_tool(ForgejoGetPrTool {
            client: client.clone(),
        })
        .with_tool(ForgejoUpdatePrTool { client })
}
