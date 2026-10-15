//! Integration tests using `wiremock` as a fake Forgejo. Tests invoke
//! `Tool::call` directly — we bypass the MCP stdio framework.
//!
//! Why each test builds a local runtime: our production code is blocking
//! (`reqwest::blocking`), which internally spins up a private tokio
//! runtime. `wiremock` is async and needs a runtime to serve HTTP. If we
//! ran the test itself under `#[tokio::test]`, dropping reqwest's private
//! runtime at shutdown panics ("Cannot drop a runtime in a context where
//! blocking is not allowed"). So: build a dedicated multi-thread runtime,
//! `block_on` only the mock setup, and run the blocking tool call on the
//! plain test thread, outside that runtime.

use std::any::Any;
use std::panic::AssertUnwindSafe;

use brenn_mcp::Tool;
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use secrecy::SecretString;

use brenn_forgejo_mcp::client::ForgejoClient;
use brenn_forgejo_mcp::redact::SecretSet;
use brenn_forgejo_mcp::tools::{
    ForgejoAddReviewCommentTool, ForgejoClosePrTool, ForgejoGetReviewCommentsTool,
    ForgejoOpenPrTool, ForgejoPostReviewTool,
};

/// Bundled runtime + mock server. The runtime serves wiremock's async
/// work; the test thread itself stays non-async so our blocking tool
/// code can run without entering the runtime's context.
struct MockEnv {
    rt: Runtime,
    server: MockServer,
}

impl MockEnv {
    fn new() -> Self {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(MockServer::start());
        MockEnv { rt, server }
    }

    /// Block on an async future that borrows the mock server.
    fn block_on<F: std::future::Future<Output = ()>>(&self, fut: F) {
        self.rt.block_on(fut);
    }

    fn uri(&self) -> String {
        self.server.uri()
    }
}

/// Build a `ForgejoClient` pointing at the given mock server. The PAT is
/// "test-token" and is registered in the scrubber's SecretSet so error
/// bodies don't leak it into `anyhow` messages.
fn mk_client(env: &MockEnv) -> ForgejoClient {
    let secrets = SecretSet::new([b"test-token".to_vec()]);
    ForgejoClient::new(
        env.uri(),
        SecretString::from("test-token"),
        "brenn-forgejo-mcp-test/0.0.1".to_string(),
        10,
        secrets,
    )
    .unwrap()
}

/// Extract the panic payload as a `String`. Shared by several panic-path
/// tests.
fn panic_msg(err: Box<dyn Any + Send>) -> String {
    if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// open_pr
// ---------------------------------------------------------------------------

#[test]
fn open_pr_happy_path() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            // F5: verify the Authorization header shape actually reaches
            // the server. This is the single `expose_secret()` call site.
            .and(header("authorization", "token test-token"))
            .and(body_json(json!({
                "title": "My PR",
                "head": "feature",
                "base": "main",
                "body": "description"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 42,
                "html_url": "https://example.com/alice/proj/pulls/42",
                "state": "open"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let result = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "My PR", "head": "feature", "base": "main",
            "body": "description"
        }))
        .unwrap();
    assert_eq!(result["number"], 42);
    assert_eq!(result["state"], "open");
}

#[test]
fn open_pr_omits_body_when_absent() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(body_json(json!({
                "title": "T", "head": "f", "base": "main"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "number": 1 })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let result = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "T", "head": "f", "base": "main"
        }))
        .unwrap();
    assert_eq!(result["number"], 1);
}

#[test]
fn open_pr_409_extracts_message_field() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "message": "head branch does not exist",
                "url": "https://example.com/api/swagger"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "T", "head": "f", "base": "main"
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (409"), "got: {s}");
    assert!(s.contains("head branch does not exist"), "got: {s}");
    // Raw body should NOT leak — we extracted `message`.
    assert!(!s.contains("swagger"), "got: {s}");
}

#[test]
fn open_pr_missing_required_field() {
    let env = MockEnv::new();
    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "a", "repo": "r", "title": "t", "head": "h" }))
        .unwrap_err();
    assert!(format!("{err:#}").contains("missing required field: base"));
}

// ---------------------------------------------------------------------------
// get_review_comments
// ---------------------------------------------------------------------------

#[test]
fn get_review_comments_merges_reviews_and_comments() {
    let env = MockEnv::new();
    env.block_on(async {
        // Page 1 of reviews: two reviews.
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 100, "state": "COMMENT", "body": "one", "user": { "login": "a" }, "submitted_at": "2026-04-20T00:00:00Z" },
                { "id": 101, "state": "APPROVED", "body": "two", "user": { "login": "b" }, "submitted_at": "2026-04-20T00:01:00Z" }
            ])))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "path": "a.rs", "body": "c1", "position": 7, "original_position": 0 },
                { "id": 2, "path": "a.rs", "body": "c2", "position": 9, "original_position": 0 },
                { "id": 3, "path": "b.rs", "body": "c3", "position": 0, "original_position": 4 }
            ])))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/101/comments",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    let reviews = out["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 2);
    // Comments merged under each review.
    let r0_comments = reviews[0]["comments"].as_array().unwrap();
    assert_eq!(r0_comments.len(), 3);
    // Field names are passed through verbatim (position, original_position).
    assert_eq!(r0_comments[0]["position"], 7);
    assert_eq!(r0_comments[2]["original_position"], 4);
    assert_eq!(reviews[1]["comments"].as_array().unwrap().len(), 0);
    // No truncation flag on small PR.
    assert!(out.get("truncated").is_none());
}

#[test]
fn get_review_comments_paginates_reviews() {
    let env = MockEnv::new();
    env.block_on(async {
        // Page 1: 50 reviews (full page, so client fetches page 2).
        let full_page: Vec<Value> = (1..=50u64)
            .map(|i| json!({ "id": i, "state": "COMMENT" }))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(full_page)))
            .mount(&env.server)
            .await;
        // Page 2: 2 reviews (partial page, stop).
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 51, "state": "COMMENT" },
                { "id": 52, "state": "COMMENT" }
            ])))
            .mount(&env.server)
            .await;
        // Mock comments endpoint for every review id we might see.
        for id in 1..=52u64 {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/v1/repos/alice/proj/pulls/5/reviews/{id}/comments"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&env.server)
                .await;
        }
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"].as_array().unwrap().len(), 52);
}

#[test]
fn get_review_comments_flags_max_response_items_truncation() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 100, "state": "COMMENT" }
            ])))
            .mount(&env.server)
            .await;
        // Exactly MAX_RESPONSE_ITEMS (50) comments → flagged as truncated.
        let fifty: Vec<Value> = (1..=50u64)
            .map(|i| json!({ "id": i, "path": "a", "body": "x" }))
            .collect();
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(fifty)))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["truncated"], true);
    let detail = out["truncated_detail"].as_str().unwrap();
    assert!(
        detail.contains("MAX_RESPONSE_ITEMS"),
        "detail was: {detail}"
    );
    // F19: multi-review capped IDs are listed as a Vec-debug; single id
    // appears as `[100]`.
    assert!(detail.contains("[100]"), "detail was: {detail}");
}

#[test]
fn get_review_comments_state_filter_applies() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "state": "COMMENT" },
                { "id": 2, "state": "APPROVED" },
                { "id": 3, "state": "COMMENT" }
            ])))
            .mount(&env.server)
            .await;
        // Only the two `COMMENT` reviews should be fetched comments for.
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews/1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews/3/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "state": "COMMENT"
        }))
        .unwrap();
    let reviews = out["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 2);
    for r in reviews {
        assert_eq!(r["state"], "COMMENT");
    }
}

// ---------------------------------------------------------------------------
// post_review
// ---------------------------------------------------------------------------

#[test]
fn post_review_without_inline_comments() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(body_json(json!({
                "event": "COMMENT",
                "body": "nice work"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 900, "state": "COMMENT"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "event": "COMMENT", "body": "nice work"
        }))
        .unwrap();
    assert_eq!(out["id"], 900);
}

#[test]
fn post_review_with_inline_comments() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(body_json(json!({
                "event": "REQUEST_CHANGES",
                "body": "see comments",
                "comments": [
                    { "path": "src/a.rs", "body": "fix this", "new_position": 42 },
                    { "path": "src/b.rs", "body": "old-side",  "old_position": 7 }
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 901, "state": "REQUEST_CHANGES"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "event": "REQUEST_CHANGES", "body": "see comments",
            "comments": [
                { "path": "src/a.rs", "body": "fix this", "new_position": 42 },
                { "path": "src/b.rs", "body": "old-side",  "old_position": 7 }
            ]
        }))
        .unwrap();
    assert_eq!(out["id"], 901);
    assert_eq!(out["state"], "REQUEST_CHANGES");
}

#[test]
fn post_review_rejects_both_positions_set() {
    let env = MockEnv::new();
    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    // Pure validation path — no HTTP. Safe to call directly.
    let err = tool
        .call(json!({
            "owner": "a", "repo": "r", "index": 1,
            "event": "COMMENT",
            "comments": [
                { "path": "f", "body": "b", "new_position": 1, "old_position": 2 }
            ]
        }))
        .unwrap_err();
    assert!(format!("{err:#}").contains("both are set"));
}

#[test]
fn post_review_rejects_no_position_set() {
    let env = MockEnv::new();
    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "a", "repo": "r", "index": 1,
            "event": "COMMENT",
            "comments": [{ "path": "f", "body": "b" }]
        }))
        .unwrap_err();
    assert!(format!("{err:#}").contains("neither is set"));
}

// ---------------------------------------------------------------------------
// close_pr
// ---------------------------------------------------------------------------

#[test]
fn close_pr_patches_state_closed() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("PATCH"))
            .and(path("/api/v1/repos/alice/proj/pulls/5"))
            .and(body_json(json!({ "state": "closed" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 5, "state": "closed"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoClosePrTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["state"], "closed");
}

// ---------------------------------------------------------------------------
// Protocol-break: 2xx with non-JSON body panics
// ---------------------------------------------------------------------------

#[test]
fn protocol_break_2xx_non_json_panics() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("PATCH"))
            .and(path("/api/v1/repos/alice/proj/pulls/5"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html>not json</html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoClosePrTool {
        client: mk_client(&env),
    };

    // Use catch_unwind to confirm the call panics. In production this
    // panic kills the process; what the MCP client does with a dead server
    // is its policy.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
    }));
    assert!(result.is_err(), "expected a panic on non-JSON 2xx body");
    let msg = panic_msg(result.err().unwrap());
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
}

/// A review object in the `/reviews` response with no numeric `id` is a
/// contract violation we can't work around — fetching per-review comments
/// needs an id. The tool panics rather than silently dropping the sub-view.
#[test]
fn get_review_comments_panics_on_review_missing_id() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "state": "COMMENT", "body": "missing id" }
            ])))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
    }));
    assert!(result.is_err(), "expected a panic on review with no id");
    let msg = panic_msg(result.err().unwrap());
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
    assert!(msg.contains("numeric `id`"), "panic payload was: {msg:?}");
}

// ---------------------------------------------------------------------------
// 5xx transient-error prefix (F4)
// ---------------------------------------------------------------------------

#[test]
fn open_pr_500_surfaces_transient_prefix() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream is sad"))
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "t", "head": "h", "base": "main"
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo transient error:"), "got: {s}");
    assert!(s.contains("500"), "got: {s}");
    // We deliberately don't embed 5xx body bytes (PAT-echo defense). The
    // body string we mocked must not appear in the error.
    assert!(
        !s.contains("upstream is sad"),
        "5xx body leaked into error: {s}"
    );
}

/// If a 4xx body happens to contain the PAT (e.g. proxy echoing the
/// Authorization header), the error string must not ship the raw PAT.
#[test]
fn client_error_body_echoing_pat_is_scrubbed() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("upstream WAF echoed header: Authorization: token test-token"),
            )
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "t", "head": "h", "base": "main"
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (400"), "got: {s}");
    assert!(!s.contains("test-token"), "PAT leaked into error: {s}");
    assert!(s.contains("<REDACTED>"), "got: {s}");
}

// ---------------------------------------------------------------------------
// Send-error classification (F26): transient prefix on connect failure
// ---------------------------------------------------------------------------

#[test]
fn send_error_on_connect_failure_surfaces_transient_prefix() {
    // No mock server — point at a port that's almost certainly closed.
    let secrets = SecretSet::new([b"test-token".to_vec()]);
    let client = ForgejoClient::new(
        "http://127.0.0.1:1".to_string(),
        SecretString::from("test-token"),
        "brenn-forgejo-mcp-test/0.0.1".to_string(),
        2, // short timeout so the test is fast
        secrets,
    )
    .unwrap();
    let tool = ForgejoClosePrTool { client };
    let err = tool
        .call(json!({ "owner": "a", "repo": "b", "index": 1 }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo transient error:"), "got: {s}");
}

// ---------------------------------------------------------------------------
// /reviews 10-page cap truncation signal (F6)
// ---------------------------------------------------------------------------

#[test]
fn get_review_comments_reports_10_page_cap() {
    let env = MockEnv::new();
    env.block_on(async {
        // Every page (1..=10) returns a full 50 reviews. The tool hits the
        // 10-page cap and sets truncated=true.
        for page in 1..=10u32 {
            let full: Vec<Value> = (((page - 1) * 50 + 1)..=(page * 50))
                .map(|i| json!({ "id": i as u64, "state": "COMMENT" }))
                .collect();
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
                .and(query_param("page", page.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(full)))
                .mount(&env.server)
                .await;
        }
        // Each review's comments endpoint returns an empty list.
        for id in 1..=500u64 {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/v1/repos/alice/proj/pulls/5/reviews/{id}/comments"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&env.server)
                .await;
        }
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"].as_array().unwrap().len(), 500);
    assert_eq!(out["truncated"], true);
    let detail = out["truncated_detail"].as_str().unwrap();
    assert!(detail.contains("10-page cap"), "detail was: {detail}");
    assert!(detail.contains("PR 5"), "detail was: {detail}");
}

// ---------------------------------------------------------------------------
// state filter: zero-match (F33)
// ---------------------------------------------------------------------------

#[test]
fn get_review_comments_state_filter_zero_match() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "state": "COMMENT" },
                { "id": 2, "state": "APPROVED" }
            ])))
            .mount(&env.server)
            .await;
        // No reviews match CHANGES_REQUESTED → no comment fetches should
        // happen; we register no comment mocks to enforce that.
    });
    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "state": "CHANGES_REQUESTED"
        }))
        .unwrap();
    assert_eq!(out["reviews"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Pagination stop-on-empty-page (F34)
// ---------------------------------------------------------------------------

#[test]
fn get_review_comments_pagination_stops_on_empty_page() {
    let env = MockEnv::new();
    env.block_on(async {
        // Page 1 full (50), page 2 empty → stop. No page 3.
        let full: Vec<Value> = (1..=50u64)
            .map(|i| json!({ "id": i, "state": "COMMENT" }))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(full)))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&env.server)
            .await;
        // Comments endpoints for ids 1..=50.
        for id in 1..=50u64 {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/v1/repos/alice/proj/pulls/5/reviews/{id}/comments"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&env.server)
                .await;
        }
        // Explicitly no mock for page=3. If the tool asks for it, wiremock
        // 404s; the tool treats 404 as a Forgejo API error (4xx) and the
        // test fails loudly.
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"].as_array().unwrap().len(), 50);
    assert!(out.get("truncated").is_none());
}

// ---------------------------------------------------------------------------
// add_review_comment
// ---------------------------------------------------------------------------

#[test]
fn add_review_comment_happy_path_new_position() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .and(header("authorization", "token test-token"))
            .and(body_json(json!({
                "path": "src/a.rs",
                "body": "looks good to me",
                "new_position": 42
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": 7000,
                "path": "src/a.rs",
                "body": "looks good to me",
                "position": 42,
                "original_position": 0
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "src/a.rs", "body": "looks good to me",
            "new_position": 42
        }))
        .unwrap();
    assert_eq!(out["id"], 7000);
    assert_eq!(out["position"], 42);
}

#[test]
fn add_review_comment_happy_path_old_position() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .and(body_json(json!({
                "path": "src/a.rs",
                "body": "pre-change side",
                "old_position": 7
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": 7001,
                "path": "src/a.rs",
                "body": "pre-change side",
                "position": 0,
                "original_position": 7
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "src/a.rs", "body": "pre-change side",
            "old_position": 7
        }))
        .unwrap();
    assert_eq!(out["id"], 7001);
    assert_eq!(out["original_position"], 7);
}

#[test]
fn add_review_comment_rejects_both_positions_set() {
    let env = MockEnv::new();
    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "a", "repo": "r", "index": 1,
            "review_id": 100,
            "path": "f", "body": "b",
            "new_position": 1, "old_position": 2
        }))
        .unwrap_err();
    assert!(format!("{err:#}").contains("both are set"));
}

#[test]
fn add_review_comment_rejects_no_position_set() {
    let env = MockEnv::new();
    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "a", "repo": "r", "index": 1,
            "review_id": 100,
            "path": "f", "body": "b"
        }))
        .unwrap_err();
    assert!(format!("{err:#}").contains("neither is set"));
}

#[test]
fn add_review_comment_missing_review_id() {
    let env = MockEnv::new();
    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "a", "repo": "r", "index": 1,
            "path": "f", "body": "b", "new_position": 1
        }))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("missing or invalid field: review_id"),
        "got: {err:#}"
    );
}

#[test]
fn add_review_comment_404_on_unknown_review_id() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/999/comments",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message": "review not found",
                "url": "https://example.com/api/swagger"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 999,
            "path": "f", "body": "b", "new_position": 1
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (404"), "got: {s}");
    assert!(s.contains("review not found"), "got: {s}");
    // Raw body's `url` field must not leak.
    assert!(!s.contains("swagger"), "got: {s}");
}

#[test]
fn add_review_comment_422_on_invalid_position() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "message": "line out of range"
            })))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "src/a.rs", "body": "b", "new_position": 9999
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (422"), "got: {s}");
    assert!(s.contains("line out of range"), "got: {s}");
}

#[test]
fn add_review_comment_500_surfaces_transient_prefix_no_body_leak() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_string("Authorization: token test-token leaked"),
            )
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "f", "body": "b", "new_position": 1
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo transient error:"), "got: {s}");
    assert!(s.contains("500"), "got: {s}");
    // 5xx body bytes are not embedded at all — PAT-echo defense.
    assert!(!s.contains("leaked"), "5xx body leaked: {s}");
    assert!(!s.contains("test-token"), "PAT leaked: {s}");
}

#[test]
fn add_review_comment_pat_scrub_on_4xx_body() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("echoed header: Authorization: token test-token"),
            )
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "f", "body": "b", "new_position": 1
        }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (400"), "got: {s}");
    assert!(!s.contains("test-token"), "PAT leaked: {s}");
    assert!(s.contains("<REDACTED>"), "got: {s}");
}

#[test]
fn add_review_comment_protocol_break_2xx_non_json_panics() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path(
                "/api/v1/repos/alice/proj/pulls/5/reviews/100/comments",
            ))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_string("<html>not json</html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoAddReviewCommentTool {
        client: mk_client(&env),
    };

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "review_id": 100,
            "path": "f", "body": "b", "new_position": 1
        }))
    }));
    assert!(result.is_err(), "expected a panic on non-JSON 2xx body");
    let msg = panic_msg(result.err().unwrap());
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
}

// ---------------------------------------------------------------------------
// post_review APPROVE with no body (F31)
// ---------------------------------------------------------------------------

/// Forgejo accepts APPROVE with no body; we do not enforce it client-side.
/// This pins the policy and guards against someone adding a local reject.
#[test]
fn post_review_approve_without_body_passes_through() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(body_json(json!({ "event": "APPROVE" })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": 1, "state": "APPROVED" })),
            )
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "event": "APPROVE"
        }))
        .unwrap();
    assert_eq!(out["state"], "APPROVED");
}
