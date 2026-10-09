# AgentDialer — conventions (AGENTS.md)

Authoritative architecture: `PLAN.md`. This file is the working-conventions
contract for anyone (human or agent) editing the code.

## Workspace shape
- Cargo workspace of **focused crates** under `crates/`. One crate = one concern.
- Cross-crate behavior lives in a shared crate; never reach into a sibling's
  internals.
- One binary (`ad_bin` → `agentdialer`) with subcommands (`api`, `orchestrator`,
  `dispatch`, `migrate`). One image, many roles.

## Hard rules
- **Docker-first, always.** Build and run everything in containers — locally and
  in CI, identically. Do not assume a host toolchain (no host `cargo`, no host
  Postgres). The test tier runs through `docker compose -f docker-compose.test.yml`.
- **Vertical slices, not phases.** Every feature ships together: domain logic +
  its API endpoint (if user-facing) + its tests, all green in the same change.
  Never "logic now, API later, tests later." If a feature has no API surface
  yet, it still ships with tests. Code without passing tests is not done.
- **Fail closed in compliance.** `ad_compliance` must never allow a call it
  cannot prove is permitted. If you need to add a code path, the default branch
  denies.
- **Pure engines are pure.** `ad_compliance` (and other decision engines) take
  data + a `Clock` and return a decision. No I/O inside decision functions.
- **Registry-driven extensibility.** Telephony providers, dial strategies,
  hopper sources, routing actions/functions, and compliance jurisdictions are
  registries. Adding a variant = its own module + a minimal, enumerated set of
  edits. No DB migration to add a variant (discriminator in data).
- **RAII/Drop over manual cleanup** for slots, claims, SIP legs.
- **Always stop the stack after tests.** When you start `docker compose up -d`
  (or any test container) you MUST run `docker compose down` as soon as the
  tests finish, including on failure. Never leave containers running between
  tasks. See the Database section for the test workflow.
- **Honest status.** `experimental` means experimental and is surfaced in APIs.
  Never overstate what works in commits/PRs/messages.

## Testing
- Tests live next to the code they test (or in the crate's `tests/`).
- A test must **fail without** the change and **pass with** it. For
  risk-critical logic, include a mutation check in review notes.
- No flaky tests: poll with a deadline, never `sleep(n)` to wait for a
  condition.
- Property tests for invariants (compliance has fail-closed/always-open ones).

## Before you say "done"
Run and pass the Docker-first test tier (fmt + clippy + full suite, including
the Postgres integration tests, all inside containers):
```
docker compose -f docker-compose.test.yml up --build \
    --abort-on-container-exit --exit-code-from tests
docker compose -f docker-compose.test.yml down        # keep cache volumes
```
CI runs the exact same compose. If you didn't run the tests, it isn't done.

## Database (Postgres)

The data layer is `ad_db` (sqlx + Postgres). The dockerized test tier above
starts Postgres as a compose service and points the integration tests at it via
`DATABASE_URL`. For an interactive local database, use `docker-compose.yml`:
```
docker compose up -d
```
The API uses Postgres when `DATABASE_URL` is set, else an in-memory store. Apply
migrations with `agentdialer migrate` (needs `DATABASE_URL`).

**Always stop the stack after tests. Never leave it up.** When you start
`docker compose up -d` for a test run, run `docker compose down` as soon as the
tests finish, including on failure. Do not leave containers running between
tasks.

## Comments
Comments must earn their place: would the next reader spend multiple tool calls
to understand this without it? If not, delete it. If a workaround needs a
paragraph to justify, fix the code.

## Writing style — ASD-STE100 (MANDATORY)
Write all explanations, comments, docs, commit messages, PR text, and chat
replies in Simplified Technical English (ASD-STE100). Aim for at least 80%
compliance. This is mandatory from now on.

Rules:
- Write short sentences. Use no more than 20 words in a procedure sentence.
- Write one instruction in one sentence.
- Use the active voice. Example: "The worker claims a lead." Not: "A lead is
  claimed."
- Use the imperative for instructions. Example: "Run the tests."
- Use simple tenses (present, past, future). Do not use continuous tenses.
- Use approved words in one meaning only. Do not use a word as both a noun and a
  verb unless the glossary permits it.
- Keep technical names and code identifiers as they are (e.g. `ad_routing`,
  `SKIP LOCKED`, `rtpengine`). ASD-STE100 does not change proper names or code.
- Do not use synonyms for variety. Use the same word for the same thing.
- Start a warning or a caution with the condition, then the instruction.
- Do not use idioms, metaphors, or filler.

Exception: this file keeps some existing text. New text follows the rule.

## Skills

The project ships Agent Skills in `.agents/skills/`. They follow the Agent
Skills open standard (`SKILL.md` per skill). They teach an AI how to use this
framework. Read `agentdialer-overview` first.

Skills:
- `agentdialer-overview` — the framework map. Read it first.
- `vertical-slice` — ship logic + API + tests together, all green.
- `compliance-engine` — work on the fail-closed compliance engine.
- `routing-engine` — build the FusionPBX-grade routing rule engine.

The skills live once in `.agents/skills/`. Each tool finds them through a
symlink: `.claude/skills`, `.gemini/skills`, and `.codex/skills` all point to
`.agents/skills`. Do not copy skills. Edit the file in `.agents/skills/`.


