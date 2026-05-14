// Compiled input-rule matcher. Built once at config-load time so the
// hot path (per-line match) does no allocation or regex recompile.

use regex::bytes::Regex;

use crate::config::{InputRuleConfig, Match};

#[derive(Debug)]
pub struct CompiledRule {
    pub matcher: Matcher,
    pub response: Vec<u8>,
}

#[derive(Debug)]
pub enum Matcher {
    Exact(Vec<u8>),
    Regex(Regex),
}

impl Matcher {
    pub fn matches(&self, line: &[u8]) -> bool {
        match self {
            Matcher::Exact(bytes) => line == bytes.as_slice(),
            Matcher::Regex(re) => re.is_match(line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InputRuleConfig, MatchConfig};

    fn exact(s: &str, resp: &str) -> InputRuleConfig {
        InputRuleConfig {
            match_: MatchConfig {
                exact: Some(s.into()),
                regex: None,
            },
            response: resp.into(),
        }
    }

    fn regex_rule(p: &str, resp: &str) -> InputRuleConfig {
        InputRuleConfig {
            match_: MatchConfig {
                exact: None,
                regex: Some(p.into()),
            },
            response: resp.into(),
        }
    }

    #[test]
    fn exact_matcher_byte_equal_only() {
        let rules = compile_rules(&[exact("Q\r\n", "X")]).unwrap();
        assert!(rules[0].matcher.matches(b"Q\r\n"));
        assert!(!rules[0].matcher.matches(b"Q\r"));
        assert!(!rules[0].matcher.matches(b"q\r\n"));
        assert!(!rules[0].matcher.matches(b"Q\r\nX"));
    }

    #[test]
    fn regex_matcher_compiles_and_matches() {
        let rules = compile_rules(&[regex_rule(r"^GET .*\r?\n$", "OK")]).unwrap();
        assert!(rules[0].matcher.matches(b"GET status\r\n"));
        assert!(rules[0].matcher.matches(b"GET \r\n"));
        assert!(!rules[0].matcher.matches(b"POST x\r\n"));
        assert!(!rules[0].matcher.matches(b"GET\r\n"), "needs literal space");
    }

    #[test]
    fn invalid_regex_reports_rule_index() {
        let err = compile_rules(&[exact("a", "x"), regex_rule("(", "y")]).unwrap_err();
        assert!(err.contains("rule 1"), "got: {}", err);
    }

    #[test]
    fn response_bytes_preserved_verbatim() {
        let rules = compile_rules(&[exact("Q", "S S  12.50 kg\r\n")]).unwrap();
        assert_eq!(rules[0].response, b"S S  12.50 kg\r\n");
    }
}

pub fn compile_rules(rules: &[InputRuleConfig]) -> Result<Vec<CompiledRule>, String> {
    rules
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let resolved = r
                .match_
                .resolve()
                .map_err(|e| format!("rule {}: {}", idx, e))?;
            let matcher = match resolved {
                Match::Exact(s) => Matcher::Exact(s.into_bytes()),
                Match::Regex(p) => Matcher::Regex(
                    Regex::new(&p)
                        .map_err(|e| format!("rule {}: regex {:?}: {}", idx, p, e))?,
                ),
            };
            Ok(CompiledRule {
                matcher,
                response: r.response.as_bytes().to_vec(),
            })
        })
        .collect()
}
