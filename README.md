# serial-mock-service

Configurable mock for serial-attached scales (and similar line-oriented devices). Opens one or more native pseudo-terminals (PTYs), exposes an HTTP API to drive them, and supports input-driven responses for end-to-end testing..

- Native PTY via `nix::pty::openpty` — no `socat`, no Docker.
- Cross-platform: macOS and Linux.
- Synchronous, single binary, ~1k lines of Rust. No async runtime.
- YAML-driven: multiple ports, multiple scenarios per port, switchable at runtime.
- SIGHUP reload — edit YAML, signal the process, PTY paths stay the same.

## Install

```sh
cargo build --release
./target/release/serial-mock-service serve path/to/config.yaml
```

On startup the service prints one line per configured port:

```
== serial-mock-service ==
port scale-1: slave = /dev/ttys002  (initial scenario: idle)
port scale-2: slave = /dev/ttys003  (initial scenario: idle)
HTTP API listening on http://127.0.0.1:5000
```

Point your serial client (`pyserial`, `screen`, `cu`, …) at the printed slave path.

## Configuration

See [`examples/config.yaml`](examples/config.yaml).

```yaml
http:
  bind: "127.0.0.1:5000"

logging:
  level: info       # error | warn | info | debug | trace
  format: text      # text | json

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
- `input_rules`: when a frame arrives from the device, walk the rules in order; the first match's `response` is written back.
- `match` is `{ exact: ... }`, `{ regex: "..." }`, or `{ mask: { pattern: ..., mask: ... } }`. Regexes use the Rust [`regex`](https://docs.rs/regex) crate (bytes mode, anchored to whatever you write — typical: `^...$`). `mask` matches byte-for-byte where the mask bit is set and ignores positions where it's clear — handy for frames with variable fields (sequence numbers, payloads).
- YAML double-quoted strings carry `\r\n` natively. No double-escaping.

Validation runs at load and on every SIGHUP — duplicate names, unknown `initial_scenario`, malformed regex, bad framing/mask parameters all abort the (re)load without disturbing running ports.

### Binary protocols

Any `match` or `response` value accepts binary, not just a plain string:

```yaml
response: "S S  12.50 kg\r\n"   # plain string -> its UTF-8 bytes (default)
response: { hex: "02 51 03" }    # whitespace / ':' / '0x' tolerated
response: { base64: "AlED" }
response: { bytes: [2, 81, 3] }  # raw byte array
```

Plain strings keep working exactly as before — only a mapping triggers binary decoding.

Binary devices rarely use `\n` framing, so a port may declare a `framing` strategy (a port-level property, independent of scenario). Absent `framing` means the legacy newline-delimited behavior.

```yaml
ports:
  - name: sensor-1
    framing: { type: delimiter, delimiter: { hex: "0D 0A" }, include_delimiter: true }
    # framing: { type: fixed, length: 8 }
    # framing: { type: length_prefixed, header_size: 2, length_offset: 1,
    #            length_size: 1, length_endian: big, length_includes: payload, trailer_size: 0 }
    # framing: { type: idle_timeout, quiet_ms: 50 }   # flush after a quiet gap
```

- `delimiter` — cut on a byte sequence (generalizes `\n`); `include_delimiter` keeps or strips it.
- `fixed` — every frame is exactly `length` bytes.
- `length_prefixed` — a header field at `length_offset` (`length_size` = 1/2/4 bytes, `length_endian`) declares the length; `length_includes` is `payload` (header + N + trailer) or `frame` (the value is the whole frame size).
- `idle_timeout` — no delimiter; accumulate bytes and emit a frame after `quiet_ms` of silence.

### Virtual USB (USB-serial / CDC-ACM)

A USB-serial adapter enumerates as an ordinary tty, so there is nothing USB-specific to mock at the byte level — point a port's `transport` at the device path and all matching/framing applies unchanged:

```yaml
transport: { type: pty }                     # default: allocate a fresh PTY
transport: { type: tty, path: /dev/ttyACM0 } # bind an existing USB-serial tty
```

On Linux this is typically `/dev/ttyACM0`, or `/dev/ttyGS0` from a `g_serial`/`dummy_hcd` gadget; on macOS a `/dev/tty.usbserial-*`. Genuine raw-USB emulation (arbitrary VID/PID, custom endpoints/control transfers) is out of scope — that needs a Linux USB gadget (configfs/FunctionFS) or USB/IP and is not possible on macOS.

### serial tools can't see the device

Since the linux and mac os x kernel does not register PTY slave devices with IOKit, they do not show up in serial port enumeration tools. You can still use the service's HTTP API to open ports by name.

```yaml
ports:
  - name: scale-1
    initial_scenario: idle
    symlink: /tmp/serial-mock/scale-1
    # ...
```

The symlink is recreated on every start (stale links from a previous run are replaced). It is removed on graceful shutdown; on `SIGKILL` it may remain, but the next start clobbers it.

### Logging

`logging.format: text` (default) is human-friendly with colors and aligned fields. `logging.format: json` emits one JSON object per line — suitable for shipping to Loki, Elasticsearch, or anything that ingests structured logs.

The `logging.level` value uses the same syntax as `RUST_LOG` (see [`tracing_subscriber::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)): plain levels (`info`, `debug`) or per-module overrides (`serial_mock_service=debug,warn`). Set the `RUST_LOG` environment variable to override the YAML at runtime without editing config.

```sh
RUST_LOG=debug ./serial-mock-service config.yaml
```

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

`matched_rule` is `"<scenario>:<rule_index>"` or `null`. `data_hex` is the entire received frame (for the default newline framing, the line including its `\n` terminator).

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

## CLI

The same binary doubles as a client. Subcommands hit the HTTP API of an already-running `serve` instance.

```sh
serial-mock-service ports                              # list all ports
serial-mock-service ports scale-1                      # detail of one port
serial-mock-service scenario scale-1 error             # switch scenario
serial-mock-service trigger scale-1 print              # fire a trigger
serial-mock-service capture events scale-1 --since 0   # read structured events
serial-mock-service capture events scale-1 --clear     # read then wipe
serial-mock-service capture raw scale-1                # raw bytes to stdout
serial-mock-service capture raw scale-1 --clear-only   # wipe buffer
```

All subcommands accept `--addr host:port` (default `127.0.0.1:5000`). Exit code is `0` on HTTP 2xx and `1` on any error or 4xx/5xx — handy in scripts. Server output goes to stdout, errors to stderr.

```sh
# Wait for a print event before continuing in a shell test
serial-mock-service trigger scale-1 print && echo "fired"
```

### Shell completion

**bash, zsh, and fish** ship with dynamic completion of port, scenario, and trigger names. The completion script shells out to `serial-mock-service complete ...` to query the running daemon, so make sure the daemon is up (and on `127.0.0.1:5000` by default, or set `--addr`) before tabbing.

```sh
# bash
serial-mock-service completions bash >> ~/.bash_completion

# zsh (assuming an entry in $fpath like ~/.zfunc)
serial-mock-service completions zsh > ~/.zfunc/_serial-mock-service
autoload -U compinit && compinit

# fish
serial-mock-service completions fish > ~/.config/fish/completions/serial-mock-service.fish
```

Example interactions (`<TAB>` marks completion):

```
serial-mock-service trigger <TAB>          # → scale-1  scale-2
serial-mock-service trigger scale-1 <TAB>  # → print  print_heavy  tare_ok
serial-mock-service scenario scale-1 <TAB> # → idle  error
```

**elvish and powershell** get the static script generated by `clap_complete` (subcommand + flag names only, no live lookups):

```sh
serial-mock-service completions elvish > ...
serial-mock-service completions --help
```

The binary must be reachable as `serial-mock-service` on `$PATH` for dynamic lookups to fire. When the daemon is down, dynamic completion silently returns nothing — falling back to whatever you type by hand.

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
├── bytes.rs      — Bytes literal (string | hex | base64 | byte array)
├── matching.rs   — compiled exact/regex/mask matchers (regex::bytes)
├── framing.rs    — frame extraction (delimiter/fixed/length-prefixed/idle)
├── transport.rs  — transport backends (PTY, existing tty)
├── capture.rs    — bounded raw ring + event log
├── port.rs       — PortState, reader thread, trigger/match helpers
├── server.rs     — PortMap (RwLock<HashMap>)
├── http.rs       — sync TcpListener, thread-per-connection, hand-rolled HTTP
└── reload.rs     — signal-hook SIGHUP thread + config diff/apply
```

Per port a dedicated thread does blocking `read()` on the transport, captures bytes, feeds them to the port's framer, and runs the active scenario's input rules against each completed frame. HTTP handlers share the same `Arc<PortState>` to fire triggers and switch scenarios.

For a PTY transport the slave fd is intentionally held open inside the service (the `keepalive` fd) so the master's blocking `read()` doesn't return EOF on macOS when no external consumer is attached.

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
