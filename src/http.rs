// Tiny synchronous HTTP/1.1 server.
//
// One std thread per connection, no keep-alive, hand-rolled request
// parsing. Designed for a localhost test rig — not the open internet.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use tracing::{debug, info, warn};

use crate::port::PortState;
use crate::server::Server;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn serve(bind: &str, server: Arc<Server>) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind)?;
    info!(bind = %bind, "HTTP API listening on http://{}", bind);
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let server = server.clone();
                thread::spawn(move || {
                    if let Err(e) = handle(stream, server) {
                        debug!(error = %e, "request handling error");
                    }
                });
            }
            Err(e) => warn!(error = %e, "accept error"),
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream, server: Arc<Server>) -> std::io::Result<()> {
    let req = match parse_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    route(&mut stream, &server, &req)
}

fn parse_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        return Ok(None);
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("").to_string();

    let (path, query) = split_query(&raw_path);

    let mut content_length = 0usize;
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = strip_header(&line, "content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Ok(None);
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        method,
        path,
        query,
        body,
    }))
}

fn split_query(raw: &str) -> (String, HashMap<String, String>) {
    match raw.split_once('?') {
        None => (raw.to_string(), HashMap::new()),
        Some((p, q)) => {
            let mut map = HashMap::new();
            for pair in q.split('&').filter(|s| !s.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                map.insert(url_decode(k), url_decode(v));
            }
            (p.to_string(), map)
        }
    }
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(n) = u8::from_str_radix(hex, 16) {
                    out.push(n);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn strip_header<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if line.len() < prefix.len() {
        return None;
    }
    if line[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

fn route(stream: &mut TcpStream, server: &Arc<Server>, req: &Request) -> std::io::Result<()> {
    let segments: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();

    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["ports"]) => list_ports(stream, server),
        ("GET", ["ports", port]) => describe_port(stream, server, port),
        ("POST", ["ports", port, "scenario"]) => switch_scenario(stream, server, port, &req.body),
        ("POST", ["ports", port, "triggers", trigger]) => {
            fire_trigger(stream, server, port, trigger)
        }
        ("GET", ["ports", port, "capture", "raw"]) => capture_raw(stream, server, port),
        ("DELETE", ["ports", port, "capture", "raw"]) => clear_raw(stream, server, port),
        ("GET", ["ports", port, "capture", "events"]) => {
            let since: u64 = req
                .query
                .get("since")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            capture_events(stream, server, port, since)
        }
        ("DELETE", ["ports", port, "capture", "events"]) => clear_events(stream, server, port),
        _ => respond_json(stream, 404, "Not Found", "{\"error\":\"not found\"}"),
    }
}

// -------- handlers --------

fn list_ports(stream: &mut TcpStream, server: &Arc<Server>) -> std::io::Result<()> {
    let mut out = String::from("[");
    let mut first = true;
    for state in server.ports.values() {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&port_summary_json(&state));
    }
    out.push(']');
    respond_json(stream, 200, "OK", &out)
}

fn describe_port(stream: &mut TcpStream, server: &Arc<Server>, name: &str) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    respond_json(stream, 200, "OK", &port_detail_json(&state))
}

fn switch_scenario(
    stream: &mut TcpStream,
    server: &Arc<Server>,
    name: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    let parsed: Result<ScenarioSwitch, _> = serde_json::from_slice(body);
    let Ok(payload) = parsed else {
        return respond_json(stream, 400, "Bad Request", "{\"error\":\"expected {\\\"scenario\\\":\\\"name\\\"}\"}");
    };
    if let Err(e) = state.switch_scenario(&payload.scenario) {
        return respond_json(
            stream,
            404,
            "Not Found",
            &format!("{{\"error\":\"{}\"}}", json_escape(&e)),
        );
    }
    respond_json(
        stream,
        200,
        "OK",
        &format!(
            "{{\"ok\":true,\"active_scenario\":\"{}\"}}",
            json_escape(&payload.scenario)
        ),
    )
}

fn fire_trigger(
    stream: &mut TcpStream,
    server: &Arc<Server>,
    port: &str,
    trigger: &str,
) -> std::io::Result<()> {
    let Some(state) = server.ports.get(port) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    match state.fire_trigger(trigger) {
        Ok(bytes) => respond_json(
            stream,
            200,
            "OK",
            &format!(
                "{{\"ok\":true,\"trigger\":\"{}\",\"bytes\":{},\"frame\":\"{}\"}}",
                json_escape(trigger),
                bytes.len(),
                json_escape(&String::from_utf8_lossy(&bytes))
            ),
        ),
        Err(e) => respond_json(
            stream,
            404,
            "Not Found",
            &format!("{{\"error\":\"{}\"}}", json_escape(&e)),
        ),
    }
}

fn capture_raw(stream: &mut TcpStream, server: &Arc<Server>, name: &str) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    let bytes = state.capture.lock().unwrap().raw_bytes();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn clear_raw(stream: &mut TcpStream, server: &Arc<Server>, name: &str) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    state.capture.lock().unwrap().clear_raw();
    respond_json(stream, 200, "OK", "{\"ok\":true}")
}

fn capture_events(
    stream: &mut TcpStream,
    server: &Arc<Server>,
    name: &str,
    since: u64,
) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    let events = state.capture.lock().unwrap().events_since(since);
    let mut out = String::from("[");
    for (i, ev) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let matched = match &ev.matched_rule {
            Some(r) => format!("\"{}\"", json_escape(r)),
            None => "null".to_string(),
        };
        out.push_str(&format!(
            "{{\"id\":{},\"ts_ms\":{},\"data_hex\":\"{}\",\"matched_rule\":{}}}",
            ev.id,
            ev.ts_ms,
            hex_encode(&ev.data),
            matched
        ));
    }
    out.push(']');
    respond_json(stream, 200, "OK", &out)
}

fn clear_events(stream: &mut TcpStream, server: &Arc<Server>, name: &str) -> std::io::Result<()> {
    let Some(state) = server.ports.get(name) else {
        return respond_json(stream, 404, "Not Found", "{\"error\":\"unknown port\"}");
    };
    state.capture.lock().unwrap().clear_events();
    respond_json(stream, 200, "OK", "{\"ok\":true}")
}

// -------- helpers --------

#[derive(serde::Deserialize)]
struct ScenarioSwitch {
    scenario: String,
}

fn port_summary_json(state: &Arc<PortState>) -> String {
    let active = state.active.read().unwrap().name.clone();
    let compiled = state.compiled.read().unwrap().clone();
    let scenarios = compiled
        .scenario_order
        .iter()
        .map(|n| format!("\"{}\"", json_escape(n)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"name\":\"{}\",\"pty_path\":\"{}\",\"active_scenario\":\"{}\",\"scenarios\":[{}]}}",
        json_escape(&state.name),
        json_escape(&state.pty_path.display().to_string()),
        json_escape(&active),
        scenarios
    )
}

fn port_detail_json(state: &Arc<PortState>) -> String {
    let scenario = state.active.read().unwrap().clone();
    let compiled = state.compiled.read().unwrap().clone();

    let triggers = scenario
        .triggers
        .keys()
        .map(|n| format!("\"{}\"", json_escape(n)))
        .collect::<Vec<_>>()
        .join(",");

    let scenarios = compiled
        .scenario_order
        .iter()
        .map(|n| format!("\"{}\"", json_escape(n)))
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "{{\"name\":\"{}\",\"pty_path\":\"{}\",\"active_scenario\":\"{}\",\"scenarios\":[{}],\"triggers\":[{}],\"input_rule_count\":{}}}",
        json_escape(&state.name),
        json_escape(&state.pty_path.display().to_string()),
        json_escape(&scenario.name),
        scenarios,
        triggers,
        scenario.input_rules.len()
    )
}

fn respond_json(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        status = status,
        status_text = status_text,
        len = body.len(),
        body = body,
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

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

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
