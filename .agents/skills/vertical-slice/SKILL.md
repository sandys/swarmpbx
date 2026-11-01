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

Take the time from the `Clock` in the shared state (`state.clock`). In
production, `serve()` gives the API an `ad_db::PgClock` (the Postgres time).
Tests give it a `FixedClock`. Do not call `Utc::now()` in a handler.

Examples:

- `crates/ad_api/src/lib.rs`: the `router(AppState)` factory.
- `crates/ad_api/src/compliance.rs`: an endpoint over a pure engine.
- `crates/ad_api/src/pacing.rs`: an endpoint that needs Postgres. It answers
  503 without Postgres, and it reads and writes settings at run time.
- `crates/ad_api/src/outcomes.rs`: the dispositions, the attempts, the
  events, and the DNC list. It shows pages (`?after=<id>&limit=<n>`), a 404
  for an unknown campaign, and a 400 for a body that does not read.

## The test pattern (in process)

Test the router with `tower::ServiceExt::oneshot`. Send a `Request`, read the
`Response`, assert the JSON contract. Use a `FixedClock` for deterministic time.

See `crates/ad_api/tests/compliance_api.rs` for the full pattern. A test that
needs Postgres reads `DATABASE_URL` and skips without it
(`crates/ad_api/tests/pacing_api.rs`). The test tier sets `DATABASE_URL`, so
the test runs there. The key helper:

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
  that covers it. The `agentdialer-overview` skill has the table ("The tests
  that a change must keep green").
- Time: a role takes the time from its `Clock`, and a call from its
  `CallTimes`. Measure a duration with `Instant`, not with two wall-clock
  times.
- The wake: start the work of the slice on an event or at a due time. If an
  API change must wake the leader, publish an event in the same transaction
  (`ad_db::events::publish_in`), and map it in `ad_orchestrator::react`. If
  work must start later, put its time into the leader's `DueTimes`. Do not
  add a poll loop or a fixed wait.
- Policy values: put a wait or a limit into the campaign's settings, with
  its default and its range in one place (for example
  `crates/ad_campaign/src/retry.rs`). The API changes it at run time. Do not
  add an environment variable for it.
- Prove the wake: `e2e/run_twilio_e2e.sh --sim` runs with the repair sweep
  at 10 min. A recipe that checks a time (`examples/outcomes.py`,
  `examples/changes.py`) then fails when an event or a due time is missing.
  Break the wake on purpose one time, and confirm that the recipe fails.
- The agent contract: the session stream is the API of the agents
  (`proto/agentdialer/session/v1/session.proto`). Add fields. Do not
  renumber or remove a field, because old agents must keep working. Test the
  server in `crates/ad_session/`, and run `e2e/run_e2e.sh`, which uses both
  reference agents.
- Phone numbers and postal codes: read them with `ad_contact` only.
- Users: if the slice changes what a user does, change the README and the
  skill that teaches it (`run-campaigns` for the API and the stack,
  `connect-an-agent` for the agent contract).

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
6. For a user-facing endpoint, start the server one time in its image, and
   send one real request from a container on the same network:
   ```
   docker build -t agentdialer:dev .
   docker network create ad-slice
   docker run -d --name ad-api --network ad-slice agentdialer:dev api
   docker run --rm --network ad-slice -v "$PWD/examples:/examples:ro" -w /examples \
     -e AD_API_URL=http://ad-api:8080 python:3.12-slim python agentdialer.py GET /health
   docker rm -f ad-api
   docker network rm ad-slice
   ```
   Without `DATABASE_URL`, the API uses in-memory stores. To test with
   Postgres, use the quickstart stack (the `run-campaigns` skill).
7. Give each new endpoint a method in the Python client
   (`examples/agentdialer.py`) and a row in README.md "The API". If it
   changes what a user does, show it in a recipe of `examples/`.
   `e2e/run_twilio_e2e.sh --sim` runs every recipe.

Do not run `cargo`, `curl`, or any other toolchain on the host.
