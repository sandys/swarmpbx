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

## Rules for the parts of a slice

- Database: if the slice changes the schema, add a new numbered migration in
  `crates/ad_db/migrations/`. Do not edit an applied migration (sqlx stores its
  checksum). Add an integration test in `crates/ad_db/tests/`. The test tier
  runs it against the compose Postgres.
- Configuration: if the slice adds, renames, or removes an environment
  variable, change `crates/ad_bin/src/config.rs`, the README "Configuration"
  table, the Helm chart, and every e2e script that starts the role. A test in
  `ad_bin` fails if one of them is out of date.
- Honest status: if an endpoint is not real yet, say so in its response.
  `POST /v1/campaigns/:id/dispatch/run-batch` returns `experimental: true` and
  names its stub initiator (`crates/ad_api/src/dispatch.rs`).
- Runtime behavior: if the slice changes how a role runs, run the e2e script
  that covers the role (`e2e/`).

## Checklist for a slice

1. Write the domain logic in its crate. Keep decision functions pure.
2. Add the API endpoint through the `router()` factory.
3. Write unit tests next to the logic and contract tests next to the API.
4. Run one crate's tests while you work. Compose starts Postgres first:
   ```
   docker compose -f docker-compose.test.yml run --rm --build tests \
       cargo test -p ad_api
   docker compose -f docker-compose.test.yml down
   ```
5. Run the full Docker test tier (fmt, clippy, all tests). All must pass:
   ```
   docker compose -f docker-compose.test.yml up --build \
       --abort-on-container-exit --exit-code-from tests
   docker compose -f docker-compose.test.yml down
   ```
6. For a user-facing endpoint, boot the server once in its image and send one
   real request from a container:
   ```
   docker build -t agentdialer:dev .
   docker run -d --name ad-api --network host \
       -e AD_API_ADDR=127.0.0.1:18080 agentdialer:dev api
   docker run --rm --network host python:3.12-slim python3 -c \
       'import urllib.request as u; print(u.urlopen("http://127.0.0.1:18080/health").read())'
   docker rm -f ad-api
   ```

Do not run `cargo`, `curl`, or any other toolchain on the host.
