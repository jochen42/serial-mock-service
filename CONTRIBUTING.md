# Contributing

Thanks for hacking on `serial-mock-service`. Quick rules of the road.

## Dev setup

Requirements:
- Rust stable (1.74+). Install via [rustup](https://rustup.rs/).
- A POSIX system with PTYs (macOS or Linux). Windows is not supported.

```sh
git clone <repo>
cd serial-mock-service
cargo build
cargo test
```

The full test suite runs in well under 5 seconds on a laptop.

## Running locally

```sh
cargo run -- examples/config.yaml
```

The service prints PTY slave paths on startup. Open them with `pyserial`, `screen`, `cu`, or any serial client.

## Code style

- `cargo fmt` before pushing. CI will reject unformatted code.
- `cargo clippy --all-targets -- -D warnings` should be clean.
- No `unwrap()` / `panic!` on user-facing or runtime paths — return `Result<_, String>` and let `main` decide. `.unwrap()` is fine in tests.
- No new top-level dependencies without justification in the PR description. We intentionally avoid async runtimes, HTTP frameworks, and logging frameworks.
- Comments explain **why**, not what. Don't restate the code.

## Testing

Two layers; both required for any non-trivial change:

1. **Unit tests** — inline `#[cfg(test)] mod tests` in each module. Use London/mockist style where there's a collaborator to fake (see `port::tests::WriteSpy`). For pure logic (matching, capture, config validation) just assert on state.
2. **Integration tests** — `tests/integration.rs`. Spawn the binary, drive HTTP, read/write the PTY slave. Each test allocates its own HTTP port and writes its own temp YAML, so tests run in parallel.

```sh
cargo test                     # everything
cargo test --bins              # unit only
cargo test --test integration  # integration only
cargo test -- --test-threads=1 # serialize when debugging flakes
```

When adding a new HTTP endpoint or YAML field, add both:
- A unit test covering the parsing / decision logic in isolation.
- An integration test that exercises it through the real binary.

## Adding a config field

1. Add the field to the relevant `*Config` struct in `src/config.rs`.
2. If it's optional, give it a serde `default` and a sensible default function.
3. Extend `validate()` if there's a constraint.
4. Wire it through to the compiled-at-load representation (`CompiledScenario`, `CompiledRule`, …) — never re-read raw config on the hot path.
5. Document it in the README and `examples/config.yaml`.
6. Test it: a unit test for validation, an integration test for runtime behavior.

## Adding an HTTP endpoint

1. Add the route arm to `route()` in `src/http.rs`. Keep the match pattern short — extract a handler function.
2. Return `respond_json(stream, status, status_text, body)`. Don't hand-build response strings ad hoc.
3. Document the endpoint in the README API table.
4. Add an integration test under `tests/integration.rs`.

## Commits & PRs

- One logical change per PR. Multiple small PRs beat one big one.
- Subject line ≤ 50 chars, imperative mood (`add foo`, not `added foo` or `adds foo`).
- Body, when present, explains *why* the change is needed and notes any non-obvious trade-offs. No need to restate the diff.
- All tests must pass on macOS and Linux. If a platform-specific quirk forces a workaround, comment why (see the `slave_keepalive` field in `src/port.rs`).

## Reporting issues

Include:
- OS + version (macOS / Linux distro).
- `rustc --version`.
- The YAML config (redacted if necessary) that triggers the bug.
- Exact reproduction steps. A failing integration test is gold.

## Out of scope

The following are deliberately not supported. PRs adding them are unlikely to land without prior discussion in an issue:
- Async runtimes (Tokio, async-std).
- HTTP frameworks (Axum, Actix, Hyper).
- TLS or auth on the HTTP API — this is a localhost test rig.
- Trigger response templating with request-body variables.
- Hot-reload via HTTP (use SIGHUP).
- Persistent capture across restarts.
- Windows support.
