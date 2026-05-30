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
    /// Wildcard byte match: `(frame[i] & mask[i]) == pattern[i] & mask[i]`
    /// for all i, with equal lengths. `pattern` is pre-masked at compile
    /// time so the hot path only masks the frame.
    Mask {
        pattern: Vec<u8>,
        mask: Vec<u8>,
    },
}

impl Matcher {
    pub fn matches(&self, line: &[u8]) -> bool {
        match self {
            Matcher::Exact(bytes) => line == bytes.as_slice(),
            Matcher::Regex(re) => re.is_match(line),
            Matcher::Mask { pattern, mask } => {
                line.len() == pattern.len()
                    && line
                        .iter()
                        .zip(pattern.iter().zip(mask.iter()))
                        .all(|(&f, (&p, &m))| (f & m) == p)
            }
        }
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
                Match::Exact(bytes) => Matcher::Exact(bytes),
                Match::Regex(p) => Matcher::Regex(
                    Regex::new(&p).map_err(|e| format!("rule {}: regex {:?}: {}", idx, p, e))?,
                ),
                Match::Mask { pattern, mask } => {
                    // Pre-mask the pattern so `matches` does one AND per byte.
                    let pattern = pattern
                        .iter()
                        .zip(mask.iter())
                        .map(|(&p, &m)| p & m)
                        .collect();
                    Matcher::Mask { pattern, mask }
                }
            };
            Ok(CompiledRule {
                matcher,
                response: r.response.0.clone(),
            })
        })
        .collect()
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
                mask: None,
            },
            response: resp.into(),
        }
    }

    fn regex_rule(p: &str, resp: &str) -> InputRuleConfig {
        InputRuleConfig {
            match_: MatchConfig {
                exact: None,
                regex: Some(p.into()),
                mask: None,
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

    fn mask_rule(pattern: Vec<u8>, mask: Vec<u8>, resp: Vec<u8>) -> InputRuleConfig {
        InputRuleConfig {
            match_: MatchConfig {
                exact: None,
                regex: None,
                mask: Some(crate::config::MaskConfig {
                    pattern: pattern.into(),
                    mask: mask.into(),
                }),
            },
            response: resp.into(),
        }
    }

    #[test]
    fn exact_matcher_works_on_binary_bytes() {
        let rule = InputRuleConfig {
            match_: MatchConfig {
                exact: Some(vec![0x02u8, 0x51, 0x03].into()),
                regex: None,
                mask: None,
            },
            response: vec![0x06u8].into(),
        };
        let rules = compile_rules(&[rule]).unwrap();
        assert!(rules[0].matcher.matches(&[0x02, 0x51, 0x03]));
        assert!(!rules[0].matcher.matches(&[0x02, 0x51]));
        assert_eq!(rules[0].response, vec![0x06]);
    }

    #[test]
    fn mask_matcher_ignores_masked_bytes() {
        // Match AA ?? 55 (middle byte don't-care).
        let rules = compile_rules(&[mask_rule(
            vec![0xAA, 0x00, 0x55],
            vec![0xFF, 0x00, 0xFF],
            vec![0x06],
        )])
        .unwrap();
        assert!(rules[0].matcher.matches(&[0xAA, 0x12, 0x55]));
        assert!(rules[0].matcher.matches(&[0xAA, 0xFF, 0x55]));
        assert!(
            !rules[0].matcher.matches(&[0xAB, 0x12, 0x55]),
            "first byte differs"
        );
        assert!(
            !rules[0].matcher.matches(&[0xAA, 0x12, 0x54]),
            "last byte differs"
        );
        assert!(!rules[0].matcher.matches(&[0xAA, 0x12]), "length differs");
    }
}
