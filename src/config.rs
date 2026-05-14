// YAML configuration: parsing + validation.
//
// The loaded `Config` is the canonical source of truth. Reload re-reads
// from disk into a fresh `Config` and atomically replaces the live one
// without panicking on errors.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub ports: Vec<PortConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Same syntax as `RUST_LOG`. Examples: `info`, `debug`,
    /// `serial_mock_service=debug,warn`.
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Deserialize, Clone)]
pub struct HttpConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self { bind: default_bind() }
    }
}

fn default_bind() -> String {
    "127.0.0.1:5000".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct PortConfig {
    pub name: String,
    pub initial_scenario: String,
    /// Optional stable symlink to the PTY slave. The service creates
    /// it on spawn and removes it on shutdown. Useful on macOS where
    /// the kernel-assigned `/dev/ttysNNN` changes every restart and
    /// is invisible to IOKit-based serial enumeration anyway — a
    /// stable path here at least lets test scripts pin to one name.
    #[serde(default)]
    pub symlink: Option<std::path::PathBuf>,
    #[serde(default)]
    pub capture: CaptureConfig,
    pub scenarios: Vec<ScenarioConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CaptureConfig {
    #[serde(default = "default_max_events")]
    pub max_events: usize,
    #[serde(default = "default_max_raw_bytes")]
    pub max_raw_bytes: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            max_events: default_max_events(),
            max_raw_bytes: default_max_raw_bytes(),
        }
    }
}

fn default_max_events() -> usize { 1000 }
fn default_max_raw_bytes() -> usize { 65536 }

#[derive(Debug, Deserialize, Clone)]
pub struct ScenarioConfig {
    pub name: String,
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,
    #[serde(default)]
    pub input_rules: Vec<InputRuleConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TriggerConfig {
    pub name: String,
    pub response: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InputRuleConfig {
    #[serde(rename = "match")]
    pub match_: MatchConfig,
    pub response: String,
}

/// YAML shape: `match: { exact: "..." }` or `match: { regex: "..." }`.
///
/// Modeled as a struct with optional fields because serde_yaml 0.9
/// expects YAML tags (`!exact`) for externally-tagged enums, which
/// is uglier in config files. Validation enforces exactly one field.
#[derive(Debug, Deserialize, Clone)]
pub struct MatchConfig {
    #[serde(default)]
    pub exact: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Match {
    Exact(String),
    Regex(String),
}

impl MatchConfig {
    pub fn resolve(&self) -> Result<Match, String> {
        match (&self.exact, &self.regex) {
            (Some(e), None) => Ok(Match::Exact(e.clone())),
            (None, Some(r)) => Ok(Match::Regex(r.clone())),
            (None, None) => Err("match: must set either `exact` or `regex`".into()),
            (Some(_), Some(_)) => Err("match: set only one of `exact` or `regex`".into()),
        }
    }
}

/// Load and validate YAML from disk. Returns a descriptive error on
/// any failure so the operator sees what to fix.
pub fn load(path: &Path) -> Result<Config, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    let cfg: Config = serde_yaml::from_str(&raw)
        .map_err(|e| format!("parse {}: {}", path.display(), e))?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<(), String> {
    let mut port_names = HashSet::new();
    for port in &cfg.ports {
        if !port_names.insert(&port.name) {
            return Err(format!("duplicate port name: {}", port.name));
        }
        if port.scenarios.is_empty() {
            return Err(format!("port {}: must declare at least one scenario", port.name));
        }
        let mut scenario_names = HashSet::new();
        let mut found_initial = false;
        for sc in &port.scenarios {
            if !scenario_names.insert(&sc.name) {
                return Err(format!(
                    "port {}: duplicate scenario name: {}",
                    port.name, sc.name
                ));
            }
            if sc.name == port.initial_scenario {
                found_initial = true;
            }
            let mut trigger_names = HashSet::new();
            for t in &sc.triggers {
                if !trigger_names.insert(&t.name) {
                    return Err(format!(
                        "port {} scenario {}: duplicate trigger: {}",
                        port.name, sc.name, t.name
                    ));
                }
            }
            // Validate each match is well-formed and regexes compile.
            for (idx, rule) in sc.input_rules.iter().enumerate() {
                let resolved = rule.match_.resolve().map_err(|e| {
                    format!("port {} scenario {} rule {}: {}", port.name, sc.name, idx, e)
                })?;
                if let Match::Regex(p) = resolved {
                    regex::Regex::new(&p).map_err(|e| {
                        format!(
                            "port {} scenario {} rule {}: invalid regex {:?}: {}",
                            port.name, sc.name, idx, p, e
                        )
                    })?;
                }
            }
        }
        if !found_initial {
            return Err(format!(
                "port {}: initial_scenario {:?} not in scenarios",
                port.name, port.initial_scenario
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Config, String> {
        let cfg: Config =
            serde_yaml::from_str(yaml).map_err(|e| format!("parse: {}", e))?;
        validate(&cfg)?;
        Ok(cfg)
    }

    const MINIMAL: &str = r#"
ports:
  - name: p1
    initial_scenario: idle
    scenarios:
      - name: idle
        triggers:
          - { name: print, response: "X\r\n" }
"#;

    #[test]
    fn minimal_config_parses() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.ports.len(), 1);
        assert_eq!(cfg.ports[0].name, "p1");
        assert_eq!(cfg.http.bind, "127.0.0.1:5000");
    }

    #[test]
    fn duplicate_port_names_rejected() {
        let yaml = r#"
ports:
  - { name: p, initial_scenario: s, scenarios: [{name: s, triggers: [], input_rules: []}] }
  - { name: p, initial_scenario: s, scenarios: [{name: s, triggers: [], input_rules: []}] }
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("duplicate port"), "{}", err);
    }

    #[test]
    fn unknown_initial_scenario_rejected() {
        let yaml = r#"
ports:
  - name: p
    initial_scenario: missing
    scenarios:
      - { name: idle, triggers: [], input_rules: [] }
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("initial_scenario"), "{}", err);
    }

    #[test]
    fn duplicate_scenario_names_rejected() {
        let yaml = r#"
ports:
  - name: p
    initial_scenario: s
    scenarios:
      - { name: s, triggers: [], input_rules: [] }
      - { name: s, triggers: [], input_rules: [] }
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("duplicate scenario"), "{}", err);
    }

    #[test]
    fn invalid_regex_rejected() {
        let yaml = r#"
ports:
  - name: p
    initial_scenario: s
    scenarios:
      - name: s
        triggers: []
        input_rules:
          - match: { regex: "(" }
            response: "X"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("invalid regex"), "{}", err);
    }

    #[test]
    fn match_with_both_fields_rejected() {
        let yaml = r#"
ports:
  - name: p
    initial_scenario: s
    scenarios:
      - name: s
        triggers: []
        input_rules:
          - match: { exact: "A", regex: "B" }
            response: "X"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("only one of"), "{}", err);
    }

    #[test]
    fn match_with_no_field_rejected() {
        let yaml = r#"
ports:
  - name: p
    initial_scenario: s
    scenarios:
      - name: s
        triggers: []
        input_rules:
          - match: {}
            response: "X"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.contains("either"), "{}", err);
    }
}
