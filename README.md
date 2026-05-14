# serial-mock-service

Configurable mock for serial-attached scales (and similar line-oriented devices). Opens one or more native pseudo-terminals (PTYs), exposes an HTTP API to drive them, and supports input-driven responses for end-to-end testing of things like [QZ Tray](https://qz.io/).

- Native PTY via `nix::pty::openpty` — no `socat`, no Docker.
- Cross-platform: macOS and Linux.
- Synchronous, single binary, ~1k lines of Rust. No async runtime.
- YAML-driven: multiple ports, multiple scenarios per port, switchable at runtime.
- SIGHUP reload — edit YAML, signal the process, PTY paths stay the same.

## Install

```sh
cargo build --release
./target/release/serial-mock-service path/to/config.yaml
```

On startup the service prints one line per configured port:

```
== serial-mock-service ==
port scale-1: slave = /dev/ttys002  (initial scenario: idle)
port scale-2: slave = /dev/ttys003  (initial scenario: idle)
HTTP API listening on http://127.0.0.1:5000
```

Point your serial client (QZ Tray, `pyserial`, `screen`, `cu`, …) at the printed slave path.

## Configuration

See [`examples/config.yaml`](examples/config.yaml).

```yaml
http:
  bind: "127.0.0.1:5000"

ports:
  - name: scale-1
    initial_scenario: idle
    capture:
      max_events: 1000
      max_raw_bytes: 65536
    scenarios:
      - name: idle
        triggers:
          - { name: print,    response: "S S  15.00 kg\r\n" }
          - { name: tare_ok,  response: "T OK\r\n" }
        input_rules:
          - match: { exact: "Q\r\n" }
            response: "S S  12.50 kg\r\n"
          - match: { regex: "^GET .*\r?\n$" }
            response: "OK\r\n"
      - name: error
        triggers:
          - { name: print, response: "ERR\r\n" }
```

- `triggers`: named static byte strings the API can push to the device on demand.
- `input_rules`: when a `\n`-terminated line arrives from the device, walk the rules in order; the first match's `response` is written back.
- `match` is either `{ exact: "..." }` or `{ regex: "..." }`. Regexes use the Rust [`regex`](https://docs.rs/regex) crate (bytes mode, anchored to whatever you write — typical: `^...$`).
- YAML double-quoted strings carry `\r\n` natively. No double-escaping.

Validation runs at load and on every SIGHUP — duplicate names, unknown `initial_scenario`, malformed regex all abort the (re)load without disturbing running ports.

## HTTP API

Localhost only by design. All bodies are JSON unless noted.

| Method | Path | Body | Effect |
|---|---|---|---|
| `GET`    | `/ports` | — | List ports: name, pty_path, active_scenario, scenarios[] |
| `GET`    | `/ports/{port}` | — | Detail of active scenario incl. triggers + rule count |
| `POST`   | `/ports/{port}/scenario` | `{"scenario":"name"}` | Switch active scenario |
| `POST`   | `/ports/{port}/triggers/{trigger}` | — | Fire trigger; writes response bytes to PTY master |
| `GET`    | `/ports/{port}/capture/raw` | — | `application/octet-stream` — all captured bytes since clear |
| `DELETE` | `/ports/{port}/capture/raw` | — | Clear raw buffer |
| `GET`    | `/ports/{port}/capture/events?since=<id>` | — | Structured event log filtered by id |
| `DELETE` | `/ports/{port}/capture/events` | — | Clear event log |

Event shape:

```json
{ "id": 42, "ts_ms": 1715692800123, "data_hex": "510d0a", "matched_rule": "idle:0" }
```

`matched_rule` is `"<scenario>:<rule_index>"` or `null`. `data_hex` is the entire received line including `\n` terminator.

### Examples

```sh
# Fire a trigger
curl -X POST http://127.0.0.1:5000/ports/scale-1/triggers/print

# Switch scenario
curl -X POST http://127.0.0.1:5000/ports/scale-1/scenario \
     -H 'Content-Type: application/json' \
     -d '{"scenario":"error"}'

# Poll capture events
curl 'http://127.0.0.1:5000/ports/scale-1/capture/events?since=0'

# Reload after editing the YAML
kill -HUP $(pgrep serial-mock-service)
```

## Reload semantics (SIGHUP)

1. Re-read + validate YAML. On any error the previous config stays active and the error is logged.
2. Existing ports get their scenarios/rules swapped under a write lock. **PTY path stays the same.** Capture buffers preserved.
3. New ports get a fresh PTY spawned; the new slave path is printed.
4. Removed ports get their master fd dropped; the reader thread exits on EOF.

## Architecture

```
src/
├── main.rs       — arg parse, port spawn, signal install, HTTP loop
├── config.rs     — YAML schema + validation
├── matching.rs   — compiled exact/regex matchers (regex::bytes)
├── capture.rs    — bounded raw ring + event log
├── port.rs       — PortState, reader thread, trigger/match helpers
├── server.rs     — PortMap (RwLock<HashMap>)
├── http.rs       — sync TcpListener, thread-per-connection, hand-rolled HTTP
└── reload.rs     — signal-hook SIGHUP thread + config diff/apply
```

Per port a dedicated thread does blocking `read()` on the PTY master, captures bytes, and on each `\n`-terminated line runs the active scenario's input rules. HTTP handlers share the same `Arc<PortState>` to fire triggers and switch scenarios.

The slave fd is intentionally held open inside the service (`slave_keepalive`) so the master's blocking `read()` doesn't return EOF on macOS when no external consumer is attached.

## Testing

```sh
cargo test                       # full suite (unit + integration)
cargo test --bins                # unit tests only
cargo test --test integration    # integration tests only
```

Unit tests follow a **London / mockist** style for the parts where collaboration matters: `port::fire_trigger_on` and `port::process_line_on` are tested against a `WriteSpy` (manual mock) so the tests verify *that the collaborator was called with the right bytes*, not internal state. Pure helpers (`matching`, `capture`, `config`) use plain state-based assertions.

Integration tests spawn the compiled binary against a temp YAML on a unique port, open the real PTY slave in raw mode, drive the HTTP API, and assert that bytes flow through the kernel end-to-end.

## License

MIT. See [`LICENSE`](LICENSE).
