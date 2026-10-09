---
name: vertical-slice
description: >
  Teaches the AgentDialer rule that every feature ships as a vertical slice:
  domain logic + its HTTP API endpoint + tests, all green in one change. Use this
  skill when you add or change any feature, when you add an API endpoint, or when
  you set up tests. Follow the axum router-factory pattern and the in-process
  contract test pattern shown here.
license: AGPL-3.0-or-later
metadata:
  project: agentdialer
  tier: project
---

# Vertical-slice delivery

Ship every feature as a vertical slice. One change contains the domain logic,
its API endpoint (if the feature is user-facing), and its tests. All of it is
green before you move on. Do not write "logic now, API later, tests later".

If a feature has no API surface yet, it still ships with tests. Code without
passing tests is not done.

## The API pattern (axum)

Put a `router()` factory in the crate. The factory returns the full
`axum::Router`. Both production and tests use the same factory. This makes the
API testable in process, with no network and no ports.

Inject time and other non-deterministic inputs through shared state. The
compliance endpoints use an injected `Clock` so tests freeze time.

See `crates/ad_api/src/lib.rs` for the `router(AppState)` factory and
`crates/ad_api/src/compliance.rs` for an endpoint over a pure engine.

## The test pattern (in process)

Test the router with `tower::ServiceExt::oneshot`. Send a `Request`, read the
`Response`, assert the JSON contract. Use a `FixedClock` for deterministic time.

See `crates/ad_api/tests/compliance_api.rs` for the full pattern. The key
helper:

```rust
async fn post_json(app: axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
```

## Test quality rules

- A test must fail without the change and pass with it. For risk-critical
  logic, break the logic on purpose once and confirm the test turns red. Then
  revert. This proves the test has teeth.
- Do not write flaky tests. Poll with a deadline. Never `sleep(n)` to wait for a
  condition.
- Write property tests for invariants. The compliance engine has fail-closed
  and always-open property tests. See `crates/ad_compliance/tests/properties.rs`.

## Checklist for a slice

1. Write the domain logic in its crate. Keep decision functions pure.
2. Add the API endpoint through the `router()` factory.
3. Write unit tests next to the logic and contract tests next to the API.
4. Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
   and `cargo test --all`. All must pass.
5. For a user-facing endpoint, boot the server once and send one real request to
   confirm the wiring. Example: `AD_API_ADDR=127.0.0.1:18080 agentdialer api`.
