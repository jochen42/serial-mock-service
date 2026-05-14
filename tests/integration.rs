// End-to-end integration tests.
//
// Each test spawns the compiled binary against a unique temp YAML on a
// unique HTTP port, opens the printed PTY slave in raw mode, drives the
// HTTP API with a hand-rolled minimal client, and asserts that real
// bytes flow through the real kernel PTY.
//
// We avoid the `serial_test` crate by giving each test its own port
// and config file. macOS opens a fresh PTY each spawn, so the slave
// paths never collide.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};

/// Allocate a non-overlapping high port per test invocation so
/// parallel `cargo test` doesn't collide.
static NEXT_PORT: AtomicU16 = AtomicU16::new(5100);

fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}

struct Service {
    child: Child,
    port: u16,
    pty_paths: Vec<String>,
    #[allow(dead_code)]
    config_path: PathBuf,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Service {
    fn spawn(yaml: &str) -> Self {
        let port = alloc_port();
        let yaml = yaml.replace("__PORT__", &port.to_string());

        let cfg_path = std::env::temp_dir().join(format!(
            "serial-mock-test-{}-{}.yaml",
            std::process::id(),
            port
        ));
        std::fs::write(&cfg_path, yaml).unwrap();

        let bin = env!("CARGO_BIN_EXE_serial-mock-service");
        let mut child = Command::new(bin)
            .arg(&cfg_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn binary");

        // Read the printed PTY paths from stdout until we see the
        // "HTTP API listening" banner.
        let mut stdout = child.stdout.take().expect("captured stdout");
        let pty_paths = read_pty_paths(&mut stdout);

        // Drain stdout in background so the child never blocks on a
        // full pipe buffer during long-running tests.
        thread::spawn(move || {
            let mut sink = [0u8; 1024];
            while stdout.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}
        });

        let svc = Service {
            child,
            port,
            pty_paths,
            config_path: cfg_path,
        };
        svc.wait_ready();
        svc
    }

    fn url(&self, path: &str) -> String {
        format!("127.0.0.1:{}{}", self.port, path)
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if TcpStream::connect(format!("127.0.0.1:{}", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("service did not become ready on port {}", self.port);
    }
}

fn read_pty_paths(stdout: &mut impl Read) -> Vec<String> {
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(stdout);
    let mut paths = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.split_once("slave = ") {
            let path = rest.1.split_whitespace().next().unwrap_or("").to_string();
            if !path.is_empty() {
                paths.push(path);
            }
        }
        if line.contains("HTTP API listening") {
            break;
        }
    }
    assert!(!paths.is_empty(), "no PTY paths printed by service");
    paths
}

/// Trivial HTTP/1.1 client. Returns `(status_code, body_bytes)`.
fn http(method: &str, host_path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let (host, path) = host_path.split_once('/').map(|(h, p)| (h.to_string(), format!("/{}", p))).unwrap();
    let mut stream = TcpStream::connect(&host).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        method = method,
        path = path,
        host = host,
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).unwrap();
    if !body.is_empty() {
        stream.write_all(body).unwrap();
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();

    // Find end-of-headers.
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("malformed response");
    let head = &buf[..split];
    let body = buf[split + 4..].to_vec();
    let status_line = std::str::from_utf8(head)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body)
}

/// Open the PTY slave in raw mode so master writes arrive verbatim.
fn open_slave_raw(path: &str) -> File {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open slave");
    let fd = f.as_raw_fd();
    // SAFETY: fd is valid for the lifetime of `f`.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut attrs = tcgetattr(borrowed).expect("tcgetattr");
    cfmakeraw(&mut attrs);
    tcsetattr(borrowed, SetArg::TCSANOW, &attrs).expect("tcsetattr");
    f
}

/// Read up to `n` bytes within `timeout`. Implementation drains a
/// background reader thread through an mpsc channel, which lets us
/// impose a deadline without needing select/poll on the slave fd.
fn read_with_timeout(slave: &mut File, n: usize, timeout: Duration) -> Vec<u8> {
    let (tx, rx) = mpsc::channel::<u8>();
    let mut reader = slave.try_clone().expect("clone slave fd");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 64];
        while !stop_clone.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(got) => {
                    for &b in &buf[..got] {
                        if tx.send(b).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while out.len() < n {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(b) => out.push(b),
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::SeqCst);
    // Best-effort: the reader thread blocks on `read`. We can't easily
    // unblock it; let it die with the test process. Detach.
    drop(handle);
    out
}

// -------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------

const SCALE_CONFIG: &str = r#"
http:
  bind: "127.0.0.1:__PORT__"

ports:
  - name: scale-1
    initial_scenario: idle
    scenarios:
      - name: idle
        triggers:
          - { name: print, response: "S S  15.00 kg\r\n" }
        input_rules:
          - match: { exact: "Q\r\n" }
            response: "S S  12.50 kg\r\n"
          - match: { regex: "^GET .*\r?\n$" }
            response: "OK\r\n"
      - name: error
        triggers:
          - { name: print, response: "ERR\r\n" }
"#;

#[test]
fn lists_configured_ports() {
    let svc = Service::spawn(SCALE_CONFIG);
    let (status, body) = http("GET", &svc.url("/ports"), &[]);
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"name\":\"scale-1\""), "{}", body);
    assert!(body.contains("\"active_scenario\":\"idle\""), "{}", body);
}

#[test]
fn trigger_writes_bytes_to_slave() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    let (status, _) = http("POST", &svc.url("/ports/scale-1/triggers/print"), &[]);
    assert_eq!(status, 200);

    let bytes = read_with_timeout(&mut slave, 15, Duration::from_secs(2));
    assert_eq!(bytes, b"S S  15.00 kg\r\n");
}

#[test]
fn scenario_switch_changes_trigger_payload() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    let (status, _) = http(
        "POST",
        &svc.url("/ports/scale-1/scenario"),
        br#"{"scenario":"error"}"#,
    );
    assert_eq!(status, 200);

    http("POST", &svc.url("/ports/scale-1/triggers/print"), &[]);
    let bytes = read_with_timeout(&mut slave, 5, Duration::from_secs(2));
    assert_eq!(bytes, b"ERR\r\n");
}

#[test]
fn input_rule_exact_fires_response() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    slave.write_all(b"Q\r\n").unwrap();
    let bytes = read_with_timeout(&mut slave, 15, Duration::from_secs(2));
    assert_eq!(bytes, b"S S  12.50 kg\r\n");
}

#[test]
fn input_rule_regex_fires_response() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    slave.write_all(b"GET status\r\n").unwrap();
    let bytes = read_with_timeout(&mut slave, 4, Duration::from_secs(2));
    assert_eq!(bytes, b"OK\r\n");
}

#[test]
fn capture_events_record_matched_rule() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    slave.write_all(b"Q\r\n").unwrap();
    // Drain the response so it doesn't interfere with subsequent tests.
    let _ = read_with_timeout(&mut slave, 32, Duration::from_millis(500));

    // Poll: the reader thread is async w.r.t. the HTTP request.
    let body = poll_events(&svc, 1, Duration::from_secs(2));
    assert!(body.contains("\"matched_rule\":\"idle:0\""), "{}", body);
    assert!(body.contains("\"data_hex\":\"510d0a\""), "{}", body);
}

#[test]
fn unknown_endpoints_return_404() {
    let svc = Service::spawn(SCALE_CONFIG);
    let (status, _) = http("GET", &svc.url("/nope"), &[]);
    assert_eq!(status, 404);
    let (status, _) = http("GET", &svc.url("/ports/missing"), &[]);
    assert_eq!(status, 404);
    let (status, _) = http(
        "POST",
        &svc.url("/ports/scale-1/triggers/ghost"),
        &[],
    );
    assert_eq!(status, 404);
}

#[test]
fn raw_capture_returns_received_bytes() {
    let svc = Service::spawn(SCALE_CONFIG);
    let mut slave = open_slave_raw(&svc.pty_paths[0]);

    slave.write_all(b"hello\n").unwrap();
    // Drain echoed response if any.
    let _ = read_with_timeout(&mut slave, 32, Duration::from_millis(300));

    // Poll for capture (reader thread is async).
    let mut found = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let (status, body) = http("GET", &svc.url("/ports/scale-1/capture/raw"), &[]);
        assert_eq!(status, 200);
        if body.windows(6).any(|w| w == b"hello\n") {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "raw capture did not contain the bytes we sent");
}

fn poll_events(svc: &Service, expected_count: usize, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        let (status, body) = http("GET", &svc.url("/ports/scale-1/capture/events?since=0"), &[]);
        assert_eq!(status, 200);
        last = String::from_utf8(body).unwrap();
        // Crude count: number of `"id":` substrings.
        let count = last.matches("\"id\":").count();
        if count >= expected_count {
            return last;
        }
        thread::sleep(Duration::from_millis(40));
    }
    last
}
