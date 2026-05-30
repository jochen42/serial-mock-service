// Per-port state and the reader thread.
//
// One `PortState` per PTY. The reader thread blocks on `read()` from
// the master fd, capturing every byte and (once a `\n`-terminated line
// is complete) walking the active scenario's input rules. The HTTP
// layer pokes the same `PortState` to fire triggers and switch
// scenarios.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use tracing::{info, warn};

use crate::capture::Capture;
use crate::config::{PortConfig, ScenarioConfig};
use crate::framing::FramingSpec;
use crate::matching::{compile_rules, CompiledRule};
use crate::transport::ReadSource;

/// Pre-compiled, ready-to-execute view of a scenario.
pub struct CompiledScenario {
    pub name: String,
    pub triggers: HashMap<String, Vec<u8>>,
    pub input_rules: Vec<CompiledRule>,
}

/// Pre-compiled view of an entire port config.
pub struct CompiledPort {
    pub scenarios: HashMap<String, Arc<CompiledScenario>>,
    pub scenario_order: Vec<String>,
}

pub struct PortState {
    pub name: String,
    /// Client-visible device path (PTY slave, or the bound tty), if any.
    pub device_path: Option<PathBuf>,
    /// Optional stable symlink to `device_path`. Created on spawn,
    /// removed on drop.
    pub symlink: Option<PathBuf>,
    /// Write half of the transport, behind a mutex so triggers and
    /// input-rule responses serialize. `Box<dyn Write>` so any backend
    /// (PTY master, tty) plugs in.
    pub writer: Mutex<Box<dyn Write + Send>>,
    pub active: RwLock<Arc<CompiledScenario>>,
    pub compiled: RwLock<Arc<CompiledPort>>,
    /// Wire framing for the reader thread. Held in an `Arc` so the reader
    /// can cheaply snapshot it each iteration, and behind an `RwLock` so
    /// reload can swap it without restarting the thread.
    pub framing: RwLock<Arc<FramingSpec>>,
    pub capture: Mutex<Capture>,
    /// Optional fd kept open to hold a kernel resource alive for the
    /// port's lifetime (the PTY slave keepalive; `None` for tty). See
    /// [`crate::transport::Opened::keepalive`].
    #[allow(dead_code)]
    keepalive: Option<OwnedFd>,
}

impl PortState {
    /// Device path as a display string for logs/JSON, or `<none>`.
    pub fn device_path_str(&self) -> String {
        self.device_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    }
}

impl Drop for PortState {
    fn drop(&mut self) {
        if let (Some(link), Some(target)) = (&self.symlink, &self.device_path) {
            // Only unlink if it actually points to our device — never
            // delete a user file by accident.
            if let Ok(actual) = std::fs::read_link(link) {
                if &actual == target {
                    let _ = std::fs::remove_file(link);
                }
            }
        }
    }
}

impl PortState {
    /// Open a PTY, compile config, spin up the reader thread, return
    /// the shared state.
    pub fn spawn(cfg: &PortConfig) -> Result<Arc<Self>, String> {
        let compiled = Arc::new(compile_port(cfg)?);
        let initial = compiled
            .scenarios
            .get(&cfg.initial_scenario)
            .cloned()
            .ok_or_else(|| format!("port {}: initial scenario missing", cfg.name))?;

        let opened = crate::transport::open(&cfg.transport)
            .map_err(|e| format!("port {}: open transport: {}", cfg.name, e))?;
        let crate::transport::Opened {
            reader,
            writer,
            device_path,
            keepalive,
        } = opened;

        // Symlink is a convenience for clients pinning to a stable
        // device path. Failure (most commonly: macOS devfs refuses
        // writes under /dev) is logged and the port still starts —
        // the real device path works regardless.
        let symlink = match (&cfg.symlink, &device_path) {
            (Some(link), Some(target)) => match create_symlink(link, target) {
                Ok(()) => Some(link.clone()),
                Err(e) => {
                    warn!(
                        port = %cfg.name,
                        link = %link.display(),
                        target = %target.display(),
                        error = %e,
                        "could not create symlink; port still usable via device path",
                    );
                    None
                }
            },
            _ => None,
        };

        let framing = Arc::new(FramingSpec::from_config(cfg.framing.as_ref()));

        let state = Arc::new(PortState {
            name: cfg.name.clone(),
            device_path,
            symlink,
            writer: Mutex::new(writer),
            active: RwLock::new(initial),
            compiled: RwLock::new(compiled),
            framing: RwLock::new(framing),
            capture: Mutex::new(Capture::new(
                cfg.capture.max_raw_bytes,
                cfg.capture.max_events,
            )),
            keepalive,
        });

        spawn_reader(state.clone(), reader);

        Ok(state)
    }

    /// Replace the compiled config for this port (used by reload).
    /// Best-effort: if the previously-active scenario no longer exists,
    /// fall back to the new initial scenario.
    pub fn swap_config(&self, cfg: &PortConfig) -> Result<(), String> {
        let compiled = Arc::new(compile_port(cfg)?);
        let new_active = {
            let current_name = self.active.read().unwrap().name.clone();
            compiled
                .scenarios
                .get(&current_name)
                .cloned()
                .or_else(|| compiled.scenarios.get(&cfg.initial_scenario).cloned())
                .ok_or_else(|| format!("port {}: no usable scenario after reload", cfg.name))?
        };
        let framing = Arc::new(FramingSpec::from_config(cfg.framing.as_ref()));
        *self.compiled.write().unwrap() = compiled;
        *self.active.write().unwrap() = new_active;
        *self.framing.write().unwrap() = framing;
        Ok(())
    }

    /// Switch active scenario by name. Returns Err if unknown.
    pub fn switch_scenario(&self, name: &str) -> Result<(), String> {
        let next = self
            .compiled
            .read()
            .unwrap()
            .scenarios
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown scenario: {}", name))?;
        *self.active.write().unwrap() = next;
        Ok(())
    }

    /// Fire a named trigger on the active scenario. Thin wrapper that
    /// locks the master and delegates to [`fire_trigger_on`] so the
    /// logic can be unit-tested against an arbitrary `Write` sink.
    pub fn fire_trigger(&self, name: &str) -> Result<Vec<u8>, String> {
        let scenario = self.active.read().unwrap().clone();
        let mut writer = self.writer.lock().unwrap();
        fire_trigger_on(&scenario, name, &mut *writer)
    }
}

/// Look up a trigger by name on the given scenario. Pure helper.
pub fn lookup_trigger<'a>(scenario: &'a CompiledScenario, name: &str) -> Option<&'a [u8]> {
    scenario.triggers.get(name).map(Vec::as_slice)
}

/// Walk input rules in order; return `(rule_index, response)` of the
/// first match, or None. Pure helper.
pub fn match_input_rule<'a>(
    scenario: &'a CompiledScenario,
    line: &[u8],
) -> Option<(usize, &'a [u8])> {
    for (idx, rule) in scenario.input_rules.iter().enumerate() {
        if rule.matcher.matches(line) {
            return Some((idx, &rule.response));
        }
    }
    None
}

/// Trigger-fire core: look up by name, write to `sink`, return bytes.
/// The `sink` parameter is the London-style seam — tests pass a
/// `Vec<u8>` spy; production passes the locked PTY master `File`.
pub fn fire_trigger_on<W: Write>(
    scenario: &CompiledScenario,
    name: &str,
    sink: &mut W,
) -> Result<Vec<u8>, String> {
    let response = lookup_trigger(scenario, name)
        .ok_or_else(|| format!("unknown trigger: {}", name))?
        .to_vec();
    sink.write_all(&response)
        .map_err(|e| format!("write master: {}", e))?;
    let _ = sink.flush();
    Ok(response)
}

/// Matched-rule label (`"<scenario>:<idx>"`) plus the response bytes
/// emitted, both optional when no rule matched.
pub type LineOutcome = (Option<String>, Option<Vec<u8>>);

/// Line-processing core: match against rules, write response (if any)
/// to `sink`. Returns the matched rule label (e.g. `"idle:0"`) and the
/// response bytes that were emitted, for the caller to push to capture.
///
/// Write failures are surfaced via `Err`; matched-rule label is still
/// returned alongside so the caller can choose to log + still record
/// the capture event.
pub fn process_line_on<W: Write>(
    scenario: &CompiledScenario,
    line: &[u8],
    sink: &mut W,
) -> Result<LineOutcome, (String, std::io::Error)> {
    match match_input_rule(scenario, line) {
        None => Ok((None, None)),
        Some((idx, response)) => {
            let resp = response.to_vec();
            let label = format!("{}:{}", scenario.name, idx);
            if let Err(e) = sink.write_all(&resp) {
                return Err((label, e));
            }
            let _ = sink.flush();
            Ok((Some(label), Some(resp)))
        }
    }
}

/// Create a symlink at `link` pointing to `target`. If `link` already
/// exists and is itself a symlink, replace it (likely a leftover from
/// a prior run); if it's a real file/dir, refuse to overwrite.
fn create_symlink(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    match std::fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::remove_file(link)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "refusing to overwrite non-symlink at requested path",
            ));
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link)
}

fn compile_port(cfg: &PortConfig) -> Result<CompiledPort, String> {
    let mut scenarios = HashMap::new();
    let mut order = Vec::with_capacity(cfg.scenarios.len());
    for sc in &cfg.scenarios {
        scenarios.insert(sc.name.clone(), Arc::new(compile_scenario(sc)?));
        order.push(sc.name.clone());
    }
    Ok(CompiledPort {
        scenarios,
        scenario_order: order,
    })
}

fn compile_scenario(sc: &ScenarioConfig) -> Result<CompiledScenario, String> {
    let triggers = sc
        .triggers
        .iter()
        .map(|t| (t.name.clone(), t.response.0.clone()))
        .collect();
    let input_rules =
        compile_rules(&sc.input_rules).map_err(|e| format!("scenario {}: {}", sc.name, e))?;
    Ok(CompiledScenario {
        name: sc.name.clone(),
        triggers,
        input_rules,
    })
}

fn spawn_reader(state: Arc<PortState>, reader: Box<dyn ReadSource>) {
    let thread_name = format!("port-reader:{}", state.name);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || reader_loop(state, reader))
        .expect("spawn reader thread");
}

fn reader_loop(state: Arc<PortState>, mut reader: Box<dyn ReadSource>) {
    let fd = reader.raw_fd();
    let mut readbuf = [0u8; 1024];
    // The framing buffer persists across reads (a frame may span chunks)
    // and across reloads (the spec swaps, the in-flight bytes don't).
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut frames: Vec<Vec<u8>> = Vec::new();
    loop {
        // Snapshot the current framing spec (cheap Arc clone). Reload can
        // swap it between iterations.
        let spec = state.framing.read().unwrap().clone();

        // Idle-timeout framing: wait with a deadline; on a quiet gap flush
        // whatever has accumulated.
        if let Some(timeout) = spec.idle_timeout() {
            if !wait_readable(fd, timeout) {
                frames.clear();
                spec.on_idle(&mut buf, &mut frames);
                for f in frames.drain(..) {
                    handle_frame(&state, f);
                }
                continue;
            }
        }

        let n = match reader.read(&mut readbuf) {
            Ok(0) => {
                info!(port = %state.name, "EOF on master, reader exiting");
                return;
            }
            Ok(n) => n,
            Err(e) => {
                warn!(port = %state.name, error = %e, "read error, reader exiting");
                return;
            }
        };
        let chunk = &readbuf[..n];

        // Always capture the raw bytes first.
        {
            let mut cap = state.capture.lock().unwrap();
            cap.append_raw(chunk);
        }

        // Feed the framer; handle each completed frame in order.
        frames.clear();
        spec.push(&mut buf, chunk, &mut frames);
        for f in frames.drain(..) {
            handle_frame(&state, f);
        }
    }
}

/// Block until `fd` is readable or `timeout` elapses. Returns true if
/// readable (or on poll error — let `read` surface it), false on timeout.
fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    // SAFETY: fd is owned by the reader's `File` for the loop's lifetime.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let pt = PollTimeout::try_from(ms).unwrap_or(PollTimeout::NONE);
    !matches!(poll(&mut fds, pt), Ok(0))
}

fn handle_frame(state: &Arc<PortState>, bytes: Vec<u8>) {
    let scenario = state.active.read().unwrap().clone();
    let matched_rule = {
        let mut writer = state.writer.lock().unwrap();
        match process_line_on(&scenario, &bytes, &mut *writer) {
            Ok((label, _resp)) => label,
            Err((label, err)) => {
                warn!(
                    port = %state.name,
                    rule = %label,
                    error = %err,
                    "input-rule write to transport failed",
                );
                Some(label)
            }
        }
    };
    state
        .capture
        .lock()
        .unwrap()
        .push_event(bytes, matched_rule);
}

#[cfg(test)]
mod tests {
    //! London-style unit tests: the trigger/match logic talks to a
    //! `Write` collaborator. Tests substitute a `Vec<u8>` spy and
    //! assert on the **bytes the collaborator was asked to write**
    //! (interaction verification), not on internal state.
    //!
    //! `WriteSpy` is a hand-rolled mock that also records call counts
    //! and can be configured to fail, so we can verify both happy and
    //! sad paths.

    use super::*;
    use crate::config::{InputRuleConfig, MatchConfig, ScenarioConfig, TriggerConfig};
    use std::io;

    /// Manual mock implementing `Write`. Captures every byte plus call
    /// counts; optionally fails the next `write_all` to exercise error
    /// paths.
    struct WriteSpy {
        pub bytes: Vec<u8>,
        pub write_calls: usize,
        pub flush_calls: usize,
        pub fail_next_write: bool,
    }

    impl WriteSpy {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                write_calls: 0,
                flush_calls: 0,
                fail_next_write: false,
            }
        }
    }

    impl io::Write for WriteSpy {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_calls += 1;
            if self.fail_next_write {
                self.fail_next_write = false;
                return Err(io::Error::other("spy: forced fail"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flush_calls += 1;
            Ok(())
        }
    }

    fn make_scenario() -> CompiledScenario {
        let cfg = ScenarioConfig {
            name: "idle".into(),
            triggers: vec![
                TriggerConfig {
                    name: "print".into(),
                    response: "S S  15.00 kg\r\n".into(),
                },
                TriggerConfig {
                    name: "tare".into(),
                    response: "T OK\r\n".into(),
                },
            ],
            input_rules: vec![
                InputRuleConfig {
                    match_: MatchConfig {
                        exact: Some("Q\r\n".into()),
                        regex: None,
                        mask: None,
                    },
                    response: "S S  12.50 kg\r\n".into(),
                },
                InputRuleConfig {
                    match_: MatchConfig {
                        exact: None,
                        regex: Some(r"^GET .*\r?\n$".into()),
                        mask: None,
                    },
                    response: "OK\r\n".into(),
                },
            ],
        };
        compile_scenario(&cfg).unwrap()
    }

    // ---- lookup_trigger / match_input_rule: pure-helper sanity ----

    #[test]
    fn lookup_trigger_returns_bytes_when_present() {
        let sc = make_scenario();
        assert_eq!(lookup_trigger(&sc, "print").unwrap(), b"S S  15.00 kg\r\n");
        assert_eq!(lookup_trigger(&sc, "tare").unwrap(), b"T OK\r\n");
    }

    #[test]
    fn lookup_trigger_none_when_unknown() {
        let sc = make_scenario();
        assert!(lookup_trigger(&sc, "nope").is_none());
    }

    #[test]
    fn match_input_rule_returns_first_match_index() {
        let sc = make_scenario();
        let (idx, resp) = match_input_rule(&sc, b"Q\r\n").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(resp, b"S S  12.50 kg\r\n");
    }

    #[test]
    fn match_input_rule_falls_through_to_regex() {
        let sc = make_scenario();
        let (idx, resp) = match_input_rule(&sc, b"GET ping\r\n").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(resp, b"OK\r\n");
    }

    #[test]
    fn match_input_rule_none_on_no_match() {
        let sc = make_scenario();
        assert!(match_input_rule(&sc, b"BOGUS\r\n").is_none());
    }

    // ---- fire_trigger_on: mockist interaction tests ----

    #[test]
    fn fire_trigger_writes_exact_bytes_to_collaborator() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        let bytes = fire_trigger_on(&sc, "print", &mut spy).unwrap();

        // Behavior we care about: collaborator got the right bytes,
        // exactly once.
        assert_eq!(spy.bytes, b"S S  15.00 kg\r\n");
        assert_eq!(spy.write_calls, 1);
        assert_eq!(bytes, b"S S  15.00 kg\r\n");
        assert!(spy.flush_calls >= 1, "expected flush after write");
    }

    #[test]
    fn fire_trigger_does_not_touch_collaborator_when_trigger_unknown() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        let err = fire_trigger_on(&sc, "nope", &mut spy).unwrap_err();
        assert!(err.contains("unknown trigger"));
        assert_eq!(spy.write_calls, 0, "no write should have been issued");
        assert!(spy.bytes.is_empty());
    }

    #[test]
    fn fire_trigger_surfaces_collaborator_write_failure() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        spy.fail_next_write = true;
        let err = fire_trigger_on(&sc, "print", &mut spy).unwrap_err();
        assert!(err.contains("write master"), "{}", err);
        // Spy was invoked; that's the interaction we wanted to verify.
        assert_eq!(spy.write_calls, 1);
    }

    // ---- process_line_on: mockist interaction tests ----

    #[test]
    fn process_line_writes_response_for_exact_match() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        let (label, resp) = process_line_on(&sc, b"Q\r\n", &mut spy).unwrap();
        assert_eq!(label.as_deref(), Some("idle:0"));
        assert_eq!(resp.as_deref(), Some(b"S S  12.50 kg\r\n".as_slice()));
        assert_eq!(spy.bytes, b"S S  12.50 kg\r\n");
        assert_eq!(spy.write_calls, 1);
    }

    #[test]
    fn process_line_writes_response_for_regex_match() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        let (label, _) = process_line_on(&sc, b"GET ping\r\n", &mut spy).unwrap();
        assert_eq!(label.as_deref(), Some("idle:1"));
        assert_eq!(spy.bytes, b"OK\r\n");
    }

    #[test]
    fn process_line_silent_when_no_rule_matches() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        let (label, resp) = process_line_on(&sc, b"BOGUS\r\n", &mut spy).unwrap();
        assert!(label.is_none());
        assert!(resp.is_none());
        assert_eq!(spy.write_calls, 0, "collaborator must not be called");
        assert!(spy.bytes.is_empty());
    }

    #[test]
    fn process_line_surfaces_write_error_with_rule_label() {
        let sc = make_scenario();
        let mut spy = WriteSpy::new();
        spy.fail_next_write = true;
        let err = process_line_on(&sc, b"Q\r\n", &mut spy).unwrap_err();
        assert_eq!(
            err.0, "idle:0",
            "label preserved so caller can still record event"
        );
        assert_eq!(spy.write_calls, 1);
    }

    // ---- PortState wiring: end-to-end with the real File seam ----

    fn test_port_cfg() -> crate::config::PortConfig {
        crate::config::PortConfig {
            name: "p".into(),
            initial_scenario: "idle".into(),
            symlink: None,
            transport: Default::default(),
            framing: None,
            capture: Default::default(),
            scenarios: vec![
                ScenarioConfig {
                    name: "idle".into(),
                    triggers: vec![TriggerConfig {
                        name: "print".into(),
                        response: "AAA\r\n".into(),
                    }],
                    input_rules: vec![],
                },
                ScenarioConfig {
                    name: "error".into(),
                    triggers: vec![TriggerConfig {
                        name: "print".into(),
                        response: "ERR\r\n".into(),
                    }],
                    input_rules: vec![],
                },
            ],
        }
    }

    #[test]
    fn switch_scenario_changes_active_arc() {
        let state = PortState::spawn(&test_port_cfg()).unwrap();
        assert_eq!(state.active.read().unwrap().name, "idle");
        state.switch_scenario("error").unwrap();
        assert_eq!(state.active.read().unwrap().name, "error");
    }

    #[test]
    fn switch_scenario_rejects_unknown() {
        let state = PortState::spawn(&test_port_cfg()).unwrap();
        let err = state.switch_scenario("ghost").unwrap_err();
        assert!(err.contains("unknown scenario"));
        assert_eq!(state.active.read().unwrap().name, "idle");
    }

    #[test]
    fn swap_config_preserves_active_when_present() {
        let state = PortState::spawn(&test_port_cfg()).unwrap();
        state.switch_scenario("error").unwrap();
        // Reload with same scenario set — active should stay `error`.
        state.swap_config(&test_port_cfg()).unwrap();
        assert_eq!(state.active.read().unwrap().name, "error");
    }

    #[test]
    fn swap_config_falls_back_to_initial_when_active_disappears() {
        let state = PortState::spawn(&test_port_cfg()).unwrap();
        state.switch_scenario("error").unwrap();

        // New config drops the `error` scenario.
        let mut new_cfg = test_port_cfg();
        new_cfg.scenarios.retain(|s| s.name != "error");
        state.swap_config(&new_cfg).unwrap();

        assert_eq!(state.active.read().unwrap().name, "idle");
    }
}
