// CLI client.
//
// `serial-mock-service serve <config>` runs the daemon (handled in
// main). Everything else here is a thin HTTP client that talks to an
// already-running instance — useful for scripting and for poking at
// the service from a shell without writing curl invocations.
//
// We hand-roll the HTTP client so we don't drag in `reqwest` /
// `ureq` for the handful of GET/POST/DELETE calls we need.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

const DEFAULT_ADDR: &str = "127.0.0.1:5000";

#[derive(Parser, Debug)]
#[command(
    name = "serial-mock-service",
    about = "Configurable mock for serial-attached scales",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the mock service (long-running daemon).
    Serve {
        /// Path to the YAML config file.
        config: PathBuf,
    },

    /// List ports and their active scenarios.
    Ports {
        /// Address of the running service (host:port).
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,

        /// Show only this port (omit to list all).
        port: Option<String>,
    },

    /// Switch the active scenario for a port.
    Scenario {
        /// Port name.
        port: String,
        /// Scenario name to make active.
        scenario: String,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },

    /// Fire a named trigger on a port.
    Trigger {
        /// Port name.
        port: String,
        /// Trigger name.
        trigger: String,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },

    /// Read the capture buffer.
    Capture {
        #[command(subcommand)]
        kind: CaptureKind,
    },

    /// Print shell completion script for the given shell to stdout.
    ///
    /// Bash / Zsh / Fish scripts include dynamic completion of port,
    /// scenario, and trigger names by shelling out to
    /// `serial-mock-service complete ...` against a running daemon.
    /// Other shells get a static completion (subcommands + flag names).
    Completions {
        /// Target shell.
        shell: Shell,
    },

    /// Internal: fetch dynamic completion candidates from a running
    /// daemon. Used by the shell completion scripts. Hidden from
    /// `--help`. Errors silently to keep tab-completion responsive.
    #[command(hide = true)]
    Complete {
        #[command(subcommand)]
        kind: CompleteKind,
    },
}

#[derive(Subcommand, Debug)]
pub enum CompleteKind {
    /// Print all port names, one per line.
    Ports {
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },
    /// Print scenario names for a given port, one per line.
    Scenarios {
        port: String,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },
    /// Print trigger names for the active scenario of a given port,
    /// one per line.
    Triggers {
        port: String,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum CaptureKind {
    /// Read raw captured bytes for a port (binary stream).
    Raw {
        port: String,
        /// Clear the buffer after reading.
        #[arg(long)]
        clear: bool,
        /// Clear without reading.
        #[arg(long, conflicts_with = "clear")]
        clear_only: bool,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },
    /// Read structured capture events (JSON).
    Events {
        port: String,
        /// Only return events with `id > since`.
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long)]
        clear: bool,
        #[arg(long, conflicts_with = "clear")]
        clear_only: bool,
        #[arg(long, default_value = DEFAULT_ADDR)]
        addr: String,
    },
}

/// Execute a client subcommand. Returns a process exit code.
pub fn run(cmd: Cmd) -> i32 {
    let result = match cmd {
        Cmd::Serve { .. } => unreachable!("serve handled in main"),
        Cmd::Ports { addr, port } => cmd_ports(&addr, port.as_deref()),
        Cmd::Scenario {
            port,
            scenario,
            addr,
        } => cmd_scenario(&addr, &port, &scenario),
        Cmd::Trigger {
            port,
            trigger,
            addr,
        } => cmd_trigger(&addr, &port, &trigger),
        Cmd::Capture { kind } => match kind {
            CaptureKind::Raw {
                port,
                clear,
                clear_only,
                addr,
            } => cmd_capture_raw(&addr, &port, clear, clear_only),
            CaptureKind::Events {
                port,
                since,
                clear,
                clear_only,
                addr,
            } => cmd_capture_events(&addr, &port, since, clear, clear_only),
        },
        Cmd::Completions { shell } => {
            print_completions(shell);
            Ok(())
        }
        Cmd::Complete { kind } => {
            cmd_complete(kind);
            Ok(())
        }
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

// -------- dynamic completion sources --------

fn cmd_complete(kind: CompleteKind) {
    // All variants are best-effort: any error (daemon down, parse
    // failure, unknown port) results in zero output rather than an
    // error message, so tab-completion stays snappy and non-noisy.
    let names = match kind {
        CompleteKind::Ports { addr } => fetch_port_names(&addr),
        CompleteKind::Scenarios { port, addr } => fetch_scenarios(&addr, &port),
        CompleteKind::Triggers { port, addr } => fetch_triggers(&addr, &port),
    };
    for n in names {
        println!("{}", n);
    }
}

fn fetch_port_names(addr: &str) -> Vec<String> {
    let body = match http(addr, "GET", "/ports", &[]) {
        Ok((200, b)) => b,
        _ => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("name")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn fetch_scenarios(addr: &str, port: &str) -> Vec<String> {
    fetch_string_array(addr, &format!("/ports/{}", port), "scenarios")
}

fn fetch_triggers(addr: &str, port: &str) -> Vec<String> {
    fetch_string_array(addr, &format!("/ports/{}", port), "triggers")
}

fn fetch_string_array(addr: &str, path: &str, field: &str) -> Vec<String> {
    let body = match http(addr, "GET", path, &[]) {
        Ok((200, b)) => b,
        _ => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get(field)
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// -------- completion script emission --------

fn print_completions(shell: Shell) {
    match shell {
        Shell::Bash => print!("{}", BASH_SCRIPT),
        Shell::Zsh => print!("{}", ZSH_SCRIPT),
        Shell::Fish => print!("{}", FISH_SCRIPT),
        other => {
            // No dynamic completion for these — fall back to clap's
            // static generator. Subcommands + flags will still work.
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(other, &mut cmd, name, &mut std::io::stdout());
        }
    }
}

const BASH_SCRIPT: &str = r#"# serial-mock-service bash completion with dynamic port/scenario/trigger lookups.
_serial_mock_service() {
    local cur prev words cword
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    local i subcmd="" subsubcmd="" addr="127.0.0.1:5000"
    local -a positionals=()

    for ((i=1; i<COMP_CWORD; i++)); do
        local w="${COMP_WORDS[i]}"
        case "$w" in
            --addr)
                addr="${COMP_WORDS[i+1]:-$addr}"
                ((i++))
                ;;
            --addr=*)
                addr="${w#--addr=}"
                ;;
            --since|--since=*|--clear|--clear-only)
                ;;
            -*)
                ;;
            *)
                if [[ -z "$subcmd" ]]; then
                    subcmd="$w"
                elif [[ "$subcmd" == "capture" && -z "$subsubcmd" ]]; then
                    subsubcmd="$w"
                elif [[ "$subcmd" == "complete" && -z "$subsubcmd" ]]; then
                    subsubcmd="$w"
                else
                    positionals+=("$w")
                fi
                ;;
        esac
    done

    local pos_count=${#positionals[@]}

    _smc_reply() { COMPREPLY=( $(compgen -W "$1" -- "$cur") ); }

    if [[ -z "$subcmd" ]]; then
        _smc_reply "serve ports scenario trigger capture completions help"
        return
    fi

    case "$subcmd" in
        serve)
            COMPREPLY=( $(compgen -f -- "$cur") )
            ;;
        ports)
            if [[ $pos_count -eq 0 && $cur != -* ]]; then
                _smc_reply "$(serial-mock-service complete ports --addr "$addr" 2>/dev/null)"
            elif [[ $cur == -* ]]; then
                _smc_reply "--addr"
            fi
            ;;
        scenario)
            case $pos_count in
                0) _smc_reply "$(serial-mock-service complete ports --addr "$addr" 2>/dev/null)" ;;
                1) _smc_reply "$(serial-mock-service complete scenarios "${positionals[0]}" --addr "$addr" 2>/dev/null)" ;;
            esac
            ;;
        trigger)
            case $pos_count in
                0) _smc_reply "$(serial-mock-service complete ports --addr "$addr" 2>/dev/null)" ;;
                1) _smc_reply "$(serial-mock-service complete triggers "${positionals[0]}" --addr "$addr" 2>/dev/null)" ;;
            esac
            ;;
        capture)
            if [[ -z "$subsubcmd" ]]; then
                _smc_reply "raw events"
            elif [[ $pos_count -eq 0 ]]; then
                _smc_reply "$(serial-mock-service complete ports --addr "$addr" 2>/dev/null)"
            else
                _smc_reply "--clear --clear-only --since --addr"
            fi
            ;;
        completions)
            _smc_reply "bash zsh fish elvish powershell"
            ;;
    esac
}
complete -F _serial_mock_service serial-mock-service
"#;

const ZSH_SCRIPT: &str = r#"#compdef serial-mock-service
# serial-mock-service zsh completion with dynamic port/scenario/trigger lookups.

_serial_mock_service() {
    local context state line
    local -a subcommands shells

    subcommands=(
        'serve:run the mock service'
        'ports:list ports or describe one'
        'scenario:switch active scenario'
        'trigger:fire a trigger'
        'capture:read capture buffer'
        'completions:emit shell completion script'
        'help:show help'
    )
    shells=(bash zsh fish elvish powershell)

    _smc_addr() {
        local i a="127.0.0.1:5000"
        for ((i=1; i<=$#words; i++)); do
            case "${words[i]}" in
                --addr) a="${words[i+1]:-$a}" ;;
                --addr=*) a="${words[i]#--addr=}" ;;
            esac
        done
        print -- "$a"
    }

    _smc_ports() {
        local a=$(_smc_addr)
        local -a p
        p=(${(@f)"$(serial-mock-service complete ports --addr $a 2>/dev/null)"})
        _describe 'port' p
    }
    _smc_scenarios() {
        local a=$(_smc_addr) p="$line[1]"
        local -a s
        s=(${(@f)"$(serial-mock-service complete scenarios $p --addr $a 2>/dev/null)"})
        _describe 'scenario' s
    }
    _smc_triggers() {
        local a=$(_smc_addr) p="$line[1]"
        local -a t
        t=(${(@f)"$(serial-mock-service complete triggers $p --addr $a 2>/dev/null)"})
        _describe 'trigger' t
    }

    _arguments -C \
        '1: :->cmd' \
        '*::arg:->args'

    case $state in
        cmd) _describe 'subcommand' subcommands ;;
        args)
            case $words[1] in
                serve) _files ;;
                ports)
                    _arguments '1::port:_smc_ports' '--addr[host:port]:address'
                    ;;
                scenario)
                    _arguments '1:port:_smc_ports' '2:scenario:_smc_scenarios' '--addr[host:port]:address'
                    ;;
                trigger)
                    _arguments '1:port:_smc_ports' '2:trigger:_smc_triggers' '--addr[host:port]:address'
                    ;;
                capture)
                    _arguments '1: :(raw events)' '2:port:_smc_ports' \
                        '--clear[clear after read]' '--clear-only[clear without reading]' \
                        '--since[only events with id > N]:N' '--addr[host:port]:address'
                    ;;
                completions)
                    _arguments "1: :($shells)"
                    ;;
            esac
            ;;
    esac
}

compdef _serial_mock_service serial-mock-service
"#;

const FISH_SCRIPT: &str = r#"# serial-mock-service fish completion with dynamic lookups.

function __smc_addr
    set -l toks (commandline -opc)
    set -l addr "127.0.0.1:5000"
    for i in (seq (count $toks))
        switch $toks[$i]
            case '--addr'
                if test (count $toks) -ge (math $i + 1)
                    set addr $toks[(math $i + 1)]
                end
            case '--addr=*'
                set addr (string sub -s 8 $toks[$i])
        end
    end
    echo $addr
end

function __smc_subcmd
    set -l toks (commandline -opc)
    if test (count $toks) -ge 2
        echo $toks[2]
    end
end

function __smc_positional_count
    set -l toks (commandline -opc)
    set -l count 0
    set -l skip 0
    for i in (seq 3 (count $toks))
        if test $skip -eq 1
            set skip 0
            continue
        end
        switch $toks[$i]
            case '--addr' '--since'
                set skip 1
            case '--*'
                # flag
            case '*'
                set count (math $count + 1)
        end
    end
    echo $count
end

function __smc_ports
    serial-mock-service complete ports --addr (__smc_addr) 2>/dev/null
end
function __smc_scenarios
    set -l toks (commandline -opc)
    set -l port ""
    if test (count $toks) -ge 3
        set port $toks[3]
    end
    test -n "$port"; and serial-mock-service complete scenarios $port --addr (__smc_addr) 2>/dev/null
end
function __smc_triggers
    set -l toks (commandline -opc)
    set -l port ""
    if test (count $toks) -ge 3
        set port $toks[3]
    end
    test -n "$port"; and serial-mock-service complete triggers $port --addr (__smc_addr) 2>/dev/null
end

# Top-level subcommands
complete -c serial-mock-service -n '__fish_use_subcommand' -a serve -d 'run the mock service'
complete -c serial-mock-service -n '__fish_use_subcommand' -a ports -d 'list ports'
complete -c serial-mock-service -n '__fish_use_subcommand' -a scenario -d 'switch scenario'
complete -c serial-mock-service -n '__fish_use_subcommand' -a trigger -d 'fire trigger'
complete -c serial-mock-service -n '__fish_use_subcommand' -a capture -d 'read capture'
complete -c serial-mock-service -n '__fish_use_subcommand' -a completions -d 'emit completion script'

# Dynamic positional completion
complete -c serial-mock-service -n '__fish_seen_subcommand_from ports; and test (__smc_positional_count) -eq 0' -a '(__smc_ports)'
complete -c serial-mock-service -n '__fish_seen_subcommand_from scenario; and test (__smc_positional_count) -eq 0' -a '(__smc_ports)'
complete -c serial-mock-service -n '__fish_seen_subcommand_from scenario; and test (__smc_positional_count) -eq 1' -a '(__smc_scenarios)'
complete -c serial-mock-service -n '__fish_seen_subcommand_from trigger; and test (__smc_positional_count) -eq 0' -a '(__smc_ports)'
complete -c serial-mock-service -n '__fish_seen_subcommand_from trigger; and test (__smc_positional_count) -eq 1' -a '(__smc_triggers)'

# capture subsubcommands
complete -c serial-mock-service -n '__fish_seen_subcommand_from capture; and not __fish_seen_subcommand_from raw events' -a 'raw events'
complete -c serial-mock-service -n '__fish_seen_subcommand_from raw events' -a '(__smc_ports)'

# completions arg
complete -c serial-mock-service -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish elvish powershell'

# --addr flag
complete -c serial-mock-service -l addr -d 'host:port of daemon' -x
complete -c serial-mock-service -l clear -d 'clear after reading'
complete -c serial-mock-service -l clear-only -d 'clear without reading'
complete -c serial-mock-service -l since -d 'only events with id > N' -x
"#;

// -------- subcommand bodies --------

fn cmd_ports(addr: &str, port: Option<&str>) -> Result<(), String> {
    let path = match port {
        Some(p) => format!("/ports/{}", p),
        None => "/ports".to_string(),
    };
    let (status, body) = http(addr, "GET", &path, &[])?;
    print_body(status, &body);
    if status >= 400 {
        return Err(format!("HTTP {}", status));
    }
    Ok(())
}

fn cmd_scenario(addr: &str, port: &str, scenario: &str) -> Result<(), String> {
    let body = format!("{{\"scenario\":\"{}\"}}", escape(scenario));
    let (status, resp) = http(
        addr,
        "POST",
        &format!("/ports/{}/scenario", port),
        body.as_bytes(),
    )?;
    print_body(status, &resp);
    if status >= 400 {
        return Err(format!("HTTP {}", status));
    }
    Ok(())
}

fn cmd_trigger(addr: &str, port: &str, trigger: &str) -> Result<(), String> {
    let (status, resp) = http(
        addr,
        "POST",
        &format!("/ports/{}/triggers/{}", port, trigger),
        &[],
    )?;
    print_body(status, &resp);
    if status >= 400 {
        return Err(format!("HTTP {}", status));
    }
    Ok(())
}

fn cmd_capture_raw(addr: &str, port: &str, clear: bool, clear_only: bool) -> Result<(), String> {
    if !clear_only {
        let (status, body) = http(addr, "GET", &format!("/ports/{}/capture/raw", port), &[])?;
        if status >= 400 {
            print_body(status, &body);
            return Err(format!("HTTP {}", status));
        }
        std::io::stdout()
            .write_all(&body)
            .map_err(|e| e.to_string())?;
    }
    if clear || clear_only {
        let (status, _) = http(addr, "DELETE", &format!("/ports/{}/capture/raw", port), &[])?;
        if status >= 400 {
            return Err(format!("clear: HTTP {}", status));
        }
    }
    Ok(())
}

fn cmd_capture_events(
    addr: &str,
    port: &str,
    since: u64,
    clear: bool,
    clear_only: bool,
) -> Result<(), String> {
    if !clear_only {
        let (status, body) = http(
            addr,
            "GET",
            &format!("/ports/{}/capture/events?since={}", port, since),
            &[],
        )?;
        print_body(status, &body);
        if status >= 400 {
            return Err(format!("HTTP {}", status));
        }
    }
    if clear || clear_only {
        let (status, _) = http(
            addr,
            "DELETE",
            &format!("/ports/{}/capture/events", port),
            &[],
        )?;
        if status >= 400 {
            return Err(format!("clear: HTTP {}", status));
        }
    }
    Ok(())
}

// -------- helpers --------

fn print_body(status: u16, body: &[u8]) {
    if status >= 400 {
        eprint!("HTTP {} ", status);
        let _ = std::io::stderr().write_all(body);
        eprintln!();
    } else {
        let _ = std::io::stdout().write_all(body);
        if body.last().copied() != Some(b'\n') {
            println!();
        }
    }
}

/// Minimal HTTP/1.1 client. Returns `(status_code, body_bytes)`.
/// `addr` is `host:port`. `path` starts with `/`.
fn http(addr: &str, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {}: {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        method = method,
        path = path,
        host = addr,
        len = body.len(),
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .map_err(|e| format!("write body: {}", e))?;
    }

    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {}", e))?;
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response: no header terminator")?;
    let head =
        std::str::from_utf8(&buf[..split]).map_err(|e| format!("non-utf8 headers: {}", e))?;
    let status_line = head.lines().next().ok_or("empty response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or("malformed status line")?
        .parse()
        .map_err(|e| format!("status parse: {}", e))?;
    Ok((status, buf[split + 4..].to_vec()))
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
