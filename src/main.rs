// Mock serial scale service.
//
// Creates a pseudo-terminal (PTY) pair and exposes a tiny HTTP API on
// 127.0.0.1:5000. A POST to /waage/print writes a formatted scale frame
// (`S S <weight> kg\r\n`) into the PTY master, so anything listening on
// the slave side (e.g. QZ Tray opening the printed /dev/ttysNNN or
// /dev/pts/N device) receives it as if it came from a real serial scale.
//
// No async runtime, no socat — just `nix` for the PTY, std for I/O and
// networking, and serde for the request body.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::sync::{Arc, Mutex};

use nix::pty::openpty;
use nix::unistd::ttyname;
use serde::Deserialize;

/// Default weight used when the request body is missing or unparseable.
const DEFAULT_WEIGHT_KG: f64 = 15.00;

/// HTTP server bind address. Localhost only; this is a test harness.
const BIND_ADDR: &str = "127.0.0.1:5000";

/// Cap incoming HTTP requests to keep memory bounded.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct PrintPayload {
    /// Weight in kilograms. German "Gewicht" matches the existing API.
    gewicht: Option<f64>,
}

fn main() {
    // 1. Allocate a PTY pair. `openpty` returns owned file descriptors
    //    for the master and the slave side. We need the slave's name
    //    to tell the operator which port to point QZ Tray at, then we
    //    can drop the slave fd — the kernel keeps the PTY alive as
    //    long as the master is open, and consumers will reopen the
    //    slave path themselves.
    let pty = openpty(None, None).expect("openpty failed — is /dev/ptmx available?");

    // `ttyname` walks /dev to resolve an fd back to a path. Works on
    // both macOS (/dev/ttysNNN) and Linux (/dev/pts/N). We deliberately
    // avoid ptsname_r because it isn't exposed uniformly by nix across
    // the two platforms.
    // `ttyname` takes anything implementing `AsFd`; `&OwnedFd` does
    // (via `BorrowedFd`), so we lend the slave fd without consuming it.
    let slave_path = ttyname(&pty.slave)
        .expect("ttyname(slave) failed — could not resolve PTY slave path");

    // Master end becomes a normal std::fs::File so we can call write().
    // `into_raw_fd` transfers ownership out of the OwnedFd; from_raw_fd
    // takes it back into the File, which will close it on drop.
    let master_fd = pty.master.into_raw_fd();
    let master_file = unsafe { File::from_raw_fd(master_fd) };

    // The slave OwnedFd is dropped here — that's fine. The slave device
    // node remains accessible via the path we just printed.
    drop(pty.slave);

    println!("== serial-mock-service ==");
    println!("PTY slave (connect QZ Tray here): {}", slave_path.display());
    println!("HTTP API listening on http://{}", BIND_ADDR);
    println!("Inject:  curl -X POST http://{}/waage/print -d '{{\"gewicht\": 12.50}}'", BIND_ADDR);

    // Share the writable master across the (single-threaded) request
    // loop. Arc<Mutex<_>> is overkill for one thread, but it costs
    // nothing and lets us trivially move to a thread-per-connection
    // model later without re-plumbing.
    let master = Arc::new(Mutex::new(master_file));

    let listener = TcpListener::bind(BIND_ADDR)
        .unwrap_or_else(|e| panic!("could not bind {BIND_ADDR}: {e}"));

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                // Errors inside a single request must never take the
                // server down — log and move on to the next client.
                if let Err(e) = handle_connection(stream, &master) {
                    eprintln!("request error: {e}");
                }
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

/// Read one HTTP/1.1 request from the socket, dispatch it, and write a
/// response. Returns Err only for unrecoverable I/O errors on the
/// socket itself; malformed requests are answered with 400 and counted
/// as a successful handling.
fn handle_connection(
    mut stream: TcpStream,
    master: &Arc<Mutex<File>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // ---- Request line ------------------------------------------------
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        // Client closed without sending anything.
        return Ok(());
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // ---- Headers (we only care about Content-Length) -----------------
    let mut content_length = 0usize;
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        header_bytes += n;
        if header_bytes > MAX_REQUEST_BYTES {
            return write_response(&mut stream, 413, "Payload Too Large", "{\"error\":\"headers too large\"}");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = strip_header_prefix(&line, "content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    if content_length > MAX_REQUEST_BYTES {
        return write_response(&mut stream, 413, "Payload Too Large", "{\"error\":\"body too large\"}");
    }

    // ---- Body --------------------------------------------------------
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    // ---- Routing -----------------------------------------------------
    match (method, path) {
        ("POST", "/waage/print") => handle_print(&mut stream, master, &body),
        _ => write_response(
            &mut stream,
            404,
            "Not Found",
            "{\"error\":\"not found\"}",
        ),
    }
}

/// Case-insensitive header prefix check. Returns the value portion if
/// the line starts with `prefix` (which must already include the colon).
fn strip_header_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if line.len() < prefix.len() {
        return None;
    }
    if line[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

/// Handle POST /waage/print: parse weight, format the scale frame,
/// write it into the PTY master, respond with JSON.
fn handle_print(
    stream: &mut TcpStream,
    master: &Arc<Mutex<File>>,
    body: &[u8],
) -> std::io::Result<()> {
    // Permissive parsing: empty body, invalid JSON, or missing field
    // all fall back to DEFAULT_WEIGHT_KG. The mock is meant to be easy
    // to drive from a browser console, not a strict API.
    let weight = if body.is_empty() {
        DEFAULT_WEIGHT_KG
    } else {
        match serde_json::from_slice::<PrintPayload>(body) {
            Ok(p) => p.gewicht.unwrap_or(DEFAULT_WEIGHT_KG),
            Err(_) => DEFAULT_WEIGHT_KG,
        }
    };

    // Scale frame: `S S <weight> kg\r\n`, weight right-aligned in a
    // 6-character field with two decimals (e.g. " 12.50", "  5.00",
    // "123.45"). Matches the format the real device emits.
    let frame = format!("S S {:>6.2} kg\r\n", weight);

    // Push the bytes into the PTY master. A short write is treated as
    // an error so the HTTP caller knows the injection failed.
    {
        let mut guard = master.lock().expect("PTY mutex poisoned");
        if let Err(e) = guard.write_all(frame.as_bytes()) {
            let body = format!("{{\"error\":\"pty write failed: {}\"}}", json_escape(&e.to_string()));
            return write_response(stream, 500, "Internal Server Error", &body);
        }
        // Flush so the slave reader sees data without waiting for the
        // kernel buffer to fill.
        let _ = guard.flush();
    }

    let response = format!(
        "{{\"ok\":true,\"weight\":{:.2},\"frame\":\"{}\"}}",
        weight,
        json_escape(frame.trim_end_matches("\r\n"))
    );
    write_response(stream, 200, "OK", &response)
}

/// Write a minimal HTTP/1.1 response with a JSON body. Connection is
/// closed after each response — we don't bother with keep-alive.
fn write_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    json_body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {json_body}",
        status = status,
        status_text = status_text,
        len = json_body.len(),
        json_body = json_body,
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Minimal JSON string escape for values we embed into hand-built
/// response bodies. Good enough for error messages and the frame echo.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
