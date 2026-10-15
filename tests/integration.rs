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
use brenn_forgejo_mcp::server::build_server;
use brenn_forgejo_mcp::tools::{
    ForgejoAddReviewCommentTool, ForgejoClosePrTool, ForgejoGetPrCommentsTool, ForgejoGetPrTool,
    ForgejoGetReviewCommentsTool, ForgejoListPrsTool, ForgejoOpenPrTool, ForgejoPostReviewTool,
    ForgejoUpdatePrTool,
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

/// Mount page `page` of the paged list endpoint at `list_path`, expected
/// never to be requested: the scan must have stopped before it.
async fn mount_unrequested_page(server: &MockServer, list_path: &str, page: u32) {
    Mock::given(method("GET"))
        .and(path(list_path))
        .and(query_param("page", page.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(server)
        .await;
}

/// Mount pages `1..=sizes.len()` of the paged list endpoint at `list_path`
/// with `sizes[i]` items each, built by `make_item` from ids numbered
/// consecutively from 1, and `X-Total-Count` set to the sum of `sizes`.
/// Every page must be requested exactly once, with `limit=50`.
async fn mount_pages(
    server: &MockServer,
    list_path: &str,
    sizes: &[u64],
    make_item: impl Fn(u64) -> Value,
) {
    let total = sizes.iter().sum();
    mount_pages_with_total(server, list_path, sizes, total, make_item).await;
}

/// `mount_pages` with an explicit `X-Total-Count`, for scans whose total
/// exceeds the items served (hidden rows, or rows beyond the page cap).
async fn mount_pages_with_total(
    server: &MockServer,
    list_path: &str,
    sizes: &[u64],
    total: u64,
    make_item: impl Fn(u64) -> Value,
) {
    let mut next = 1u64;
    for (i, &n) in sizes.iter().enumerate() {
        let items: Vec<Value> = (next..next + n).map(&make_item).collect();
        next += n;
        Mock::given(method("GET"))
            .and(path(list_path))
            .and(query_param("page", (i + 1).to_string()))
            .and(query_param("limit", "50"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", total.to_string())
                    .set_body_json(Value::Array(items)),
            )
            .expect(1)
            .mount(server)
            .await;
    }
}

/// A `/reviews` entry with id `id` and state `COMMENT`.
fn review(id: u64) -> Value {
    json!({ "id": id, "state": "COMMENT" })
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
            .respond_with(ResponseTemplate::new(200).insert_header("X-Total-Count", "2").set_body_json(json!([
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
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
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
        // Page 1: 50 reviews; page 2: 2 reviews; X-Total-Count 52 ends the
        // scan after page 2.
        mount_pages(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[50, 2],
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 3).await;
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
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1")
                    .set_body_json(json!([
                        { "id": 100, "state": "COMMENT" }
                    ])),
            )
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
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
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
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "3")
                    .set_body_json(json!([
                        { "id": 1, "state": "COMMENT" },
                        { "id": 2, "state": "APPROVED" },
                        { "id": 3, "state": "COMMENT" }
                    ])),
            )
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
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
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
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1")
                    .set_body_json(json!([
                        { "state": "COMMENT", "body": "missing id" }
                    ])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
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
        // Every page (1..=10) returns a full 50 reviews of 600. The tool hits
        // the 10-page cap and sets truncated=true.
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[50; 10],
            600,
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 11).await;
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
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "2")
                    .set_body_json(json!([
                        { "id": 1, "state": "COMMENT" },
                        { "id": 2, "state": "APPROVED" }
                    ])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
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
// Pagination stop by X-Total-Count
// ---------------------------------------------------------------------------

#[test]
fn get_review_comments_stops_when_total_covered() {
    let env = MockEnv::new();
    env.block_on(async {
        // Page 1 holds all 50 of 50 rows → stop. No page 2.
        mount_pages(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[50],
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
        mount_empty_review_comments(&env.server, 1..=50).await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"].as_array().unwrap().len(), 50);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

/// A server that clamps the page size to 20 regardless of `limit`: the
/// short `/reviews` pages do not end the scan; `X-Total-Count` 45 does.
#[test]
fn get_review_comments_clamped_page_size_yields_full_set() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[20, 20, 5],
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 4).await;
        for id in 1..=45u64 {
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
    assert_eq!(out["reviews"].as_array().unwrap().len(), 45);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

/// A server that clamps the page size to 20: ten pages of 20 cannot cover
/// 300 rows, so the scan hits the cap and flags truncation; page 11 is
/// never asked.
#[test]
fn get_review_comments_clamped_page_size_at_cap_flags_truncation() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[20; 10],
            300,
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 11).await;
        for id in 1..=200u64 {
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
    assert_eq!(out["reviews"].as_array().unwrap().len(), 200);
    assert_eq!(out["truncated"], true);
    assert_eq!(
        out["truncated_detail"],
        "hit 10-page cap on reviews for PR 5"
    );
    env.block_on(env.server.verify());
}

/// Mount an empty `/reviews/{id}/comments` response for every id in `ids`.
async fn mount_empty_review_comments(server: &MockServer, ids: impl IntoIterator<Item = u64>) {
    for id in ids {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v1/repos/alice/proj/pulls/5/reviews/{id}/comments"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }
}

/// Page 1 is two other users' pending reviews, hidden after the page was
/// cut, so it comes back `[]`; `X-Total-Count` 4 says rows remain, and
/// page 2's submitted reviews are returned.
#[test]
fn get_review_comments_scans_past_hidden_pending_page() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[0, 2],
            4,
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 3).await;
        mount_empty_review_comments(&env.server, 1..=2).await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    let ids: Vec<&Value> = out["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["id"])
        .collect();
    assert_eq!(ids, vec![&json!(1), &json!(2)]);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

/// One colleague mid-review on a PR with no submitted reviews: page 1 is
/// `[]` with `X-Total-Count` 1. One request, empty result, no flag.
#[test]
fn get_review_comments_single_hidden_draft_is_empty_unflagged() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[0],
            1,
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 2).await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"], json!([]));
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

/// Ten empty pages at a page size of at least one cannot cover 50 rows, so
/// the cap is reported.
#[test]
fn get_review_comments_all_hidden_flags_cap() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls/5/reviews",
            &[0; 10],
            50,
            review,
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls/5/reviews", 11).await;
    });

    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out["reviews"], json!([]));
    assert_eq!(out["truncated"], true);
    assert_eq!(
        out["truncated_detail"],
        "hit 10-page cap on reviews for PR 5"
    );
    env.block_on(env.server.verify());
}

/// Call `forgejo_get_review_comments` against a page 1 built by `resp` and
/// return the panic message.
fn review_page_panic(resp: ResponseTemplate) -> String {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/5/reviews"))
            .and(query_param("page", "1"))
            .respond_with(resp)
            .mount(&env.server)
            .await;
    });
    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
    }));
    assert!(result.is_err(), "expected a panic");
    panic_msg(result.err().unwrap())
}

#[test]
fn get_review_comments_panics_without_total_count() {
    let msg = review_page_panic(ResponseTemplate::new(200).set_body_json(json!([review(1)])));
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
    assert!(
        msg.contains("without X-Total-Count"),
        "panic payload was: {msg:?}"
    );
}

#[test]
fn get_review_comments_panics_on_non_numeric_total_count() {
    let msg = review_page_panic(
        ResponseTemplate::new(200)
            .insert_header("X-Total-Count", "many")
            .set_body_json(json!([review(1)])),
    );
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
    assert!(
        msg.contains("non-numeric X-Total-Count"),
        "panic payload was: {msg:?}"
    );
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

#[test]
fn open_pr_rejects_non_string_body() {
    let env = MockEnv::new();
    let tool = ForgejoOpenPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj",
            "title": "T", "head": "f", "base": "main",
            "body": 42
        }))
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "field body must be a string");
}

#[test]
fn post_review_rejects_non_string_body() {
    let env = MockEnv::new();
    let tool = ForgejoPostReviewTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "event": "APPROVED", "body": 123
        }))
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "field body must be a string");
}

#[test]
fn get_review_comments_rejects_non_string_state() {
    let env = MockEnv::new();
    let tool = ForgejoGetReviewCommentsTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5, "state": 5 }))
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "field state must be a string");
}

// ---------------------------------------------------------------------------
// get_pr_comments
// ---------------------------------------------------------------------------

#[test]
fn get_pr_comments_happy_path() {
    let env = MockEnv::new();
    let comments = json!([
        { "id": 1, "body": "please rename", "user": { "login": "bob" },
          "pull_request_url": "https://example.com/alice/proj/pulls/5" },
        { "id": 2, "body": "done", "user": { "login": "alice" } }
    ]);
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/issues/5/comments"))
            .and(header("authorization", "token test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(comments.clone()))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 5 }))
        .unwrap();
    assert_eq!(out, json!({ "comments": comments }));

    let reqs = env.rt.block_on(env.server.received_requests()).unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].url.query(),
        None,
        "no query params when since absent"
    );
}

#[test]
fn get_pr_comments_passes_since() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/issues/5/comments"))
            .and(query_param("since", "2026-10-01T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "id": 3 }])))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrCommentsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({
            "owner": "alice", "repo": "proj", "index": 5,
            "since": "2026-10-01T00:00:00Z"
        }))
        .unwrap();
    assert_eq!(out["comments"][0]["id"], 3);
}

#[test]
fn get_pr_comments_404_surfaces_api_error() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/issues/999/comments"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "issue not found" })),
            )
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrCommentsTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 999 }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo API error (404"), "got: {s}");
    assert!(s.contains("issue not found"), "got: {s}");
}

// ---------------------------------------------------------------------------
// list_prs
// ---------------------------------------------------------------------------

/// A raw Forgejo PR object with the nested `repo` blocks and a `body`, so
/// tests can check the projection drops them.
fn raw_pr(number: u64, head_ref: &str) -> Value {
    json!({
        "id": 1000 + number,
        "number": number,
        "title": format!("PR {number}"),
        "body": "a long description",
        "state": "open",
        "merged": false,
        "html_url": format!("https://example.com/alice/proj/pulls/{number}"),
        "user": { "id": 1, "login": "alice", "email": "a@example.com" },
        "head": {
            "label": head_ref, "ref": head_ref, "sha": format!("sha-{number}"), "repo_id": 5,
            "repo": { "id": 5, "name": "proj", "owner": { "login": "alice" } }
        },
        "base": {
            "label": "main", "ref": "main", "sha": "base-sha", "repo_id": 5,
            "repo": { "id": 5, "name": "proj", "owner": { "login": "alice" } }
        }
    })
}

#[test]
fn list_prs_default_state_open() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("state", "open"))
            .and(query_param("page", "1"))
            .and(query_param("limit", "50"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1")
                    .set_body_json(json!([raw_pr(7, "feature")])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 2).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj" }))
        .unwrap();
    let pulls = out["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1);
    let pr = &pulls[0];
    assert_eq!(pr["number"], 7);
    assert_eq!(pr["head"]["sha"], "sha-7");
    assert_eq!(pr["base"]["ref"], "main");
    assert_eq!(pr["user"], json!({ "id": 1, "login": "alice" }));
    assert!(pr.get("body").is_none(), "body must not be returned: {pr}");
    assert!(pr["head"].get("repo").is_none(), "head.repo dropped: {pr}");
    assert!(pr["base"].get("repo").is_none(), "base.repo dropped: {pr}");
    assert!(out.get("truncated").is_none());
}

#[test]
fn list_prs_state_all_passed_through() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("state", "all"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1")
                    .set_body_json(json!([raw_pr(2, "x")])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 2).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "state": "all" }))
        .unwrap();
    assert_eq!(out["pulls"][0]["number"], 2);
    assert!(out.get("truncated").is_none());
}

#[test]
fn list_prs_rejects_unknown_state() {
    let env = MockEnv::new();
    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "state": "merged" }))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("state must be one of open, closed, all"),
        "got: {err:#}"
    );
}

#[test]
fn list_prs_head_filter_matches_label_exactly() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "4")
                    .set_body_json(json!([
                        raw_pr(1, "other"),
                        raw_pr(2, "stack/part-2"),
                        raw_pr(3, "stack/part-2-extra"),
                        raw_pr(4, "alice:stack/part-2")
                    ])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 2).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "stack/part-2" }))
        .unwrap();
    let pulls = out["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "got: {out}");
    assert_eq!(pulls[0]["number"], 2);
}

#[test]
fn list_prs_head_filter_matches_label_after_branch_deleted() {
    // Forgejo reports `head.ref` as `refs/pull/N/head` once the head branch
    // is deleted; `head.label` keeps the branch name.
    let mut merged = raw_pr(7, "stack/part-1");
    merged["state"] = json!("closed");
    merged["merged"] = json!(true);
    merged["head"]["ref"] = json!("refs/pull/7/head");
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("state", "all"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "2")
                    .set_body_json(json!([raw_pr(8, "other"), merged])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 2).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "state": "all", "head": "stack/part-1" }))
        .unwrap();
    let pulls = out["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "got: {out}");
    assert_eq!(pulls[0]["number"], 7);
    assert_eq!(pulls[0]["merged"], true);
    assert_eq!(pulls[0]["head"]["ref"], "refs/pull/7/head");
    assert!(out.get("truncated").is_none());
}

#[test]
fn list_prs_head_filter_collects_matches_across_pages() {
    let env = MockEnv::new();
    env.block_on(async {
        let mut page1: Vec<Value> = (1..=49u64).map(|i| raw_pr(i, "unrelated")).collect();
        page1.insert(10, raw_pr(100, "wanted"));
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "52")
                    .set_body_json(Value::Array(page1)),
            )
            .expect(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "52")
                    .set_body_json(json!([raw_pr(51, "unrelated"), raw_pr(52, "wanted")])),
            )
            .expect(1)
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 3).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "wanted" }))
        .unwrap();
    let numbers: Vec<u64> = out["pulls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, vec![100, 52]);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

#[test]
fn list_prs_rejects_empty_head() {
    let env = MockEnv::new();
    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "" }))
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "head must not be empty");
}

/// Page 1 is clamped to 20 of 22 entries, so the null (the 22nd entry
/// overall) is at page 2 position 2, not at page 1 position 22.
#[test]
fn list_prs_null_entry_is_error_naming_position() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls",
            &[20],
            22,
            |k| raw_pr(k, "b"),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "22")
                    .set_body_json(json!([raw_pr(21, "b"), null])),
            )
            .expect(1)
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 3).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj" }))
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "Forgejo returned null for the PR at page 2 position 2 of alice/proj pulls (state=open): it could not render that PR (server-side failure; see Forgejo logs)"
    );
    env.block_on(env.server.verify());
}

#[test]
fn list_prs_stops_when_total_covered() {
    let env = MockEnv::new();
    env.block_on(async {
        let full: Vec<Value> = (1..=50u64).map(|i| raw_pr(i, "b")).collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "52")
                    .set_body_json(Value::Array(full)),
            )
            .expect(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "52")
                    .set_body_json(json!([raw_pr(51, "b"), raw_pr(52, "b")])),
            )
            .expect(1)
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 3).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj" }))
        .unwrap();
    assert_eq!(out["pulls"].as_array().unwrap().len(), 52);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

#[test]
fn list_prs_reports_page_cap_with_head_hint() {
    let env = MockEnv::new();
    env.block_on(async {
        for page in 1..=10u64 {
            let full: Vec<Value> = (((page - 1) * 50 + 1)..=(page * 50))
                .map(|i| raw_pr(i, "unrelated"))
                .collect();
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/alice/proj/pulls"))
                .and(query_param("page", page.to_string()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("X-Total-Count", "600")
                        .set_body_json(Value::Array(full)),
                )
                .mount(&env.server)
                .await;
        }
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 11).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "wanted" }))
        .unwrap();
    assert_eq!(out["pulls"], json!([]));
    assert_eq!(out["truncated"], true);
    let detail = out["truncated_detail"].as_str().unwrap();
    assert!(detail.contains("10-page cap"), "detail was: {detail}");
    assert!(
        detail.contains("a PR with head `wanted` may exist beyond the scanned pages"),
        "detail was: {detail}"
    );
}

#[test]
fn list_prs_panics_on_entry_missing_head() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1")
                    .set_body_json(json!([
                        { "number": 9, "base": { "ref": "main" } }
                    ])),
            )
            .mount(&env.server)
            .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 2).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({ "owner": "alice", "repo": "proj" }))
    }));
    assert!(result.is_err(), "expected a panic on PR with no head");
    let msg = panic_msg(result.err().unwrap());
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
    assert!(msg.contains("`head`"), "panic payload was: {msg:?}");
}

/// A server that clamps the page size to 20 regardless of `limit`: pages
/// are short, and the scan continues until `X-Total-Count` 45 is covered.
#[test]
fn list_prs_clamped_page_size_yields_full_set() {
    let env = MockEnv::new();
    env.block_on(async {
        // PR 43 is on page 3 (PRs 41..=45).
        mount_pages(
            &env.server,
            "/api/v1/repos/alice/proj/pulls",
            &[20, 20, 5],
            |k| raw_pr(k, if k == 43 { "wanted" } else { "unrelated" }),
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 4).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "wanted" }))
        .unwrap();
    let pulls = out["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "got: {out}");
    assert_eq!(pulls[0]["number"], 43);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

#[test]
fn list_prs_clamped_page_size_at_cap_flags_truncation() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages_with_total(
            &env.server,
            "/api/v1/repos/alice/proj/pulls",
            &[20; 10],
            300,
            |k| raw_pr(k, "unrelated"),
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 11).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "head": "wanted" }))
        .unwrap();
    assert_eq!(out["pulls"], json!([]));
    assert_eq!(out["truncated"], true);
    let detail = out["truncated_detail"].as_str().unwrap();
    assert!(detail.contains("200 PRs scanned"), "detail was: {detail}");
    assert!(
        detail.contains("a PR with head `wanted` may exist beyond the scanned pages"),
        "detail was: {detail}"
    );
    env.block_on(env.server.verify());
}

/// 500 PRs fill exactly ten pages of 50: the scan is complete at the cap
/// and not flagged.
#[test]
fn list_prs_exact_multiple_at_cap_is_not_flagged() {
    let env = MockEnv::new();
    env.block_on(async {
        mount_pages(
            &env.server,
            "/api/v1/repos/alice/proj/pulls",
            &[50; 10],
            |k| raw_pr(k, "b"),
        )
        .await;
        mount_unrequested_page(&env.server, "/api/v1/repos/alice/proj/pulls", 11).await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj" }))
        .unwrap();
    assert_eq!(out["pulls"].as_array().unwrap().len(), 500);
    assert!(out.get("truncated").is_none());
    env.block_on(env.server.verify());
}

#[test]
fn list_prs_panics_on_non_numeric_total_count() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls"))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "many")
                    .set_body_json(json!([raw_pr(1, "b")])),
            )
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoListPrsTool {
        client: mk_client(&env),
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tool.call(json!({ "owner": "alice", "repo": "proj" }))
    }));
    assert!(
        result.is_err(),
        "expected a panic on non-numeric X-Total-Count"
    );
    let msg = panic_msg(result.err().unwrap());
    assert!(msg.contains("protocol break"), "panic payload was: {msg:?}");
    assert!(
        msg.contains("non-numeric X-Total-Count"),
        "panic payload was: {msg:?}"
    );
}

// ---------------------------------------------------------------------------
// get_pr
// ---------------------------------------------------------------------------

#[test]
fn get_pr_returns_full_object() {
    let env = MockEnv::new();
    let pr = raw_pr(7, "feature");
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/7"))
            .and(header("authorization", "token test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr.clone()))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrTool {
        client: mk_client(&env),
    };
    let out = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 7 }))
        .unwrap();
    assert_eq!(out, pr);
    assert_eq!(out["head"]["repo"]["name"], "proj");
    assert_eq!(out["body"], "a long description");
}

#[test]
fn get_pr_404_on_issue_index() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/3"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "not found" })),
            )
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 3 }))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo API error (404"), "got: {s}");
}

#[test]
fn get_pr_null_body_is_error() {
    let env = MockEnv::new();
    env.block_on(async {
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/alice/proj/pulls/4"))
            .respond_with(ResponseTemplate::new(200).set_body_string("null"))
            .mount(&env.server)
            .await;
    });

    let tool = ForgejoGetPrTool {
        client: mk_client(&env),
    };
    let err = tool
        .call(json!({ "owner": "alice", "repo": "proj", "index": 4 }))
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "Forgejo returned null for PR alice/proj#4: it could not render that PR (server-side failure; see Forgejo logs)"
    );
}

const UPDATE_PATH: &str = "/api/v1/repos/alice/proj/pulls/5";

fn update_tool(env: &MockEnv) -> ForgejoUpdatePrTool {
    ForgejoUpdatePrTool {
        client: mk_client(env),
    }
}

/// Mount a PATCH that matches `expected` exactly and answers 201 with `resp`.
fn mount_update(env: &MockEnv, expected: Value, resp: Value) {
    env.block_on(async {
        Mock::given(method("PATCH"))
            .and(path(UPDATE_PATH))
            .and(body_json(expected))
            .respond_with(ResponseTemplate::new(201).set_body_json(resp))
            .expect(1)
            .mount(&env.server)
            .await;
    });
}

/// Mount a PATCH that answers `status` with `body`, for any payload.
fn mount_update_status(env: &MockEnv, status: u16, body: &str) {
    env.block_on(async {
        Mock::given(method("PATCH"))
            .and(path(UPDATE_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&env.server)
            .await;
    });
}

/// Mount a catch-all that must never be hit: input errors send no request.
fn mount_no_request(env: &MockEnv) {
    env.block_on(async {
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(0)
            .mount(&env.server)
            .await;
    });
}

fn update_args(extra: Value) -> Value {
    let mut args = json!({ "owner": "alice", "repo": "proj", "index": 5 });
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

#[test]
fn update_pr_title_only() {
    let env = MockEnv::new();
    let resp = json!({ "number": 5, "title": "New", "base": { "ref": "main" } });
    mount_update(&env, json!({ "title": "New" }), resp.clone());
    let out = update_tool(&env)
        .call(update_args(json!({ "title": "New" })))
        .unwrap();
    assert_eq!(out, resp);
}

#[test]
fn update_pr_title_and_body() {
    let env = MockEnv::new();
    let resp = json!({ "number": 5, "title": "New", "body": "Desc", "base": { "ref": "main" } });
    mount_update(
        &env,
        json!({ "title": "New", "body": "Desc" }),
        resp.clone(),
    );
    let out = update_tool(&env)
        .call(update_args(json!({ "title": "New", "body": "Desc" })))
        .unwrap();
    assert_eq!(out, resp);
}

#[test]
fn update_pr_base_only() {
    let env = MockEnv::new();
    let resp = json!({ "number": 5, "base": { "ref": "dev", "sha": "abc" } });
    mount_update(&env, json!({ "base": "dev" }), resp.clone());
    let out = update_tool(&env)
        .call(update_args(json!({ "base": "dev" })))
        .unwrap();
    assert_eq!(out, resp);
}

#[test]
fn update_pr_empty_body_clears() {
    let env = MockEnv::new();
    let resp = json!({ "number": 5, "body": "" });
    mount_update(&env, json!({ "body": "" }), resp.clone());
    let out = update_tool(&env)
        .call(update_args(json!({ "body": "" })))
        .unwrap();
    assert_eq!(out, resp);
}

fn update_input_error(extra: Value) -> String {
    let env = MockEnv::new();
    mount_no_request(&env);
    let err = update_tool(&env).call(update_args(extra)).unwrap_err();
    format!("{err:#}")
}

#[test]
fn update_pr_rejects_no_fields() {
    assert_eq!(
        update_input_error(json!({})),
        "at least one of title, body or base is required"
    );
}

#[test]
fn update_pr_rejects_empty_title() {
    assert_eq!(
        update_input_error(json!({ "title": "" })),
        "title must not be empty"
    );
}

#[test]
fn update_pr_rejects_empty_base() {
    assert_eq!(
        update_input_error(json!({ "base": "" })),
        "base must not be empty"
    );
}

#[test]
fn update_pr_rejects_base_with_title_or_body() {
    for extra in [
        json!({ "base": "dev", "title": "T" }),
        json!({ "base": "dev", "body": "B" }),
        json!({ "base": "dev", "title": "T", "body": "B" }),
    ] {
        let s = update_input_error(extra);
        assert_eq!(
            s,
            "base cannot be combined with title or body in one call; change the base first, then the title/body"
        );
    }
}

#[test]
fn update_pr_409_surfaces_api_error_message() {
    let env = MockEnv::new();
    mount_update_status(
        &env,
        409,
        r#"{"message": "pull request already exists for these targets"}"#,
    );
    let err = update_tool(&env)
        .call(update_args(json!({ "base": "dev" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo API error (409"), "got: {s}");
    assert!(s.contains("already exists"), "got: {s}");
    assert!(!s.contains("note:"), "got: {s}");
}

#[test]
fn update_pr_base_unchanged_in_201_is_error() {
    let env = MockEnv::new();
    mount_update(
        &env,
        json!({ "base": "dev" }),
        json!({ "number": 5, "merged": true, "base": { "ref": "main" } }),
    );
    let err = update_tool(&env)
        .call(update_args(json!({ "base": "dev" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("`main`"), "got: {s}");
    assert!(s.contains("`dev`"), "got: {s}");
    assert!(s.contains("merged"), "got: {s}");
}

#[test]
fn update_pr_base_matches_in_201_is_ok() {
    let env = MockEnv::new();
    let resp = json!({ "number": 5, "base": { "ref": "main" } });
    mount_update(&env, json!({ "base": "main" }), resp.clone());
    let out = update_tool(&env)
        .call(update_args(json!({ "base": "main" })))
        .unwrap();
    assert_eq!(out, resp);
}

#[test]
fn update_pr_panics_on_201_without_base_ref() {
    let env = MockEnv::new();
    mount_update(&env, json!({ "base": "dev" }), json!({ "number": 5 }));
    let tool = update_tool(&env);
    let err = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = tool.call(update_args(json!({ "base": "dev" })));
    }))
    .unwrap_err();
    assert!(panic_msg(err).contains("protocol break"));
}

#[test]
fn update_pr_500_with_base_keeps_prefix_and_appends_note() {
    let env = MockEnv::new();
    mount_update_status(&env, 500, "internal error");
    let err = update_tool(&env)
        .call(update_args(json!({ "base": "feature" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo transient error:"), "got: {s}");
    assert!(s.contains("500"), "got: {s}");
    assert!(
        s.contains("same commit as the PR's head"),
        "missing same-commit note: {s}"
    );
}

#[test]
fn update_pr_500_title_only_has_no_note() {
    let env = MockEnv::new();
    mount_update_status(&env, 500, "internal error");
    let err = update_tool(&env)
        .call(update_args(json!({ "title": "T" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo transient error:"), "got: {s}");
    assert!(!s.contains("note:"), "got: {s}");
}

#[test]
fn update_pr_500_transient_no_body_leak() {
    let env = MockEnv::new();
    mount_update_status(&env, 500, "Authorization: token test-token leaked");
    let err = update_tool(&env)
        .call(update_args(json!({ "base": "dev" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.starts_with("Forgejo transient error:"), "got: {s}");
    assert!(!s.contains("leaked"), "5xx body leaked: {s}");
    assert!(!s.contains("test-token"), "PAT leaked: {s}");
}

#[test]
fn update_pr_pat_scrub_on_4xx_body() {
    let env = MockEnv::new();
    mount_update_status(&env, 400, "echoed header: Authorization: token test-token");
    let err = update_tool(&env)
        .call(update_args(json!({ "title": "T" })))
        .unwrap_err();
    let s = format!("{err:#}");
    assert!(s.contains("Forgejo API error (400"), "got: {s}");
    assert!(!s.contains("test-token"), "PAT leaked: {s}");
    assert!(s.contains("<REDACTED>"), "got: {s}");
}

#[test]
fn server_registers_every_tool() {
    let env = MockEnv::new();
    let server = build_server(mk_client(&env));
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let mut out = Vec::new();
    server
        .serve(format!("{request}\n").as_bytes(), &mut out)
        .unwrap();
    let resp: Value = serde_json::from_slice(&out).unwrap();
    let names: Vec<&str> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "forgejo_open_pr",
            "forgejo_get_review_comments",
            "forgejo_post_review",
            "forgejo_add_review_comment",
            "forgejo_close_pr",
            "forgejo_get_pr_comments",
            "forgejo_list_prs",
            "forgejo_get_pr",
            "forgejo_update_pr",
        ]
    );
}
