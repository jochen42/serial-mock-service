// Config-level byte literal.
//
// Historically every match/response in the YAML was a plain string, and
// its UTF-8 bytes went on the wire. That can't express arbitrary binary
// frames (e.g. `0x02 0x51 0x03`), so `Bytes` accepts either:
//
//   - a plain YAML scalar          -> its UTF-8 bytes (unchanged behavior)
//   - a single-key encoding map:
//       { hex:    "02 51 03" }     -> whitespace / ':' / '0x' tolerated
//       { base64: "AlED" }
//       { utf8:   "Q\r\n" }        -> explicit form of the plain scalar
//       { bytes:  [2, 81, 3] }     -> raw byte array
//
// Backward compatibility hinges on the scalar-vs-map distinction: a bare
// string stays a string, only a mapping triggers binary decoding.

use serde::de::{self, Deserializer};
use serde::Deserialize;

use base64::Engine;

/// A decoded byte literal from config. The inner `Vec<u8>` is what goes
/// on the wire verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl From<&str> for Bytes {
    fn from(s: &str) -> Self {
        Bytes(s.as_bytes().to_vec())
    }
}

impl From<String> for Bytes {
    fn from(s: String) -> Self {
        Bytes(s.into_bytes())
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}

/// Decode a hex string, ignoring ASCII whitespace, `:` separators and an
/// optional leading `0x`. Returns a human-readable error on bad input.
pub fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        // Skip a `0x` / `0X` prefix wherever it appears between bytes.
        if c == '0' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some('x') | Some('X')) {
                chars.next();
                chars.next();
                continue;
            }
        }
        if c.is_ascii_whitespace() || c == ':' || c == '_' {
            chars.next();
            continue;
        }
        let digit = c
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex character {:?}", c))?;
        nibbles.push(digit as u8);
        chars.next();
    }
    if !nibbles.len().is_multiple_of(2) {
        return Err(format!("hex has odd number of digits ({})", nibbles.len()));
    }
    Ok(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

// Intermediate representation serde deserializes into, before we collapse
// it to bytes. `untagged` lets a YAML scalar pick `Plain` while a mapping
// picks `Tagged`.
#[derive(Deserialize)]
#[serde(untagged)]
enum BytesRepr {
    Plain(String),
    Tagged(TaggedBytes),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggedBytes {
    #[serde(default)]
    hex: Option<String>,
    #[serde(default)]
    base64: Option<String>,
    #[serde(default)]
    utf8: Option<String>,
    #[serde(default)]
    bytes: Option<Vec<u8>>,
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let repr = BytesRepr::deserialize(d)?;
        let bytes = match repr {
            BytesRepr::Plain(s) => s.into_bytes(),
            BytesRepr::Tagged(t) => {
                // Exactly one encoding key must be set.
                let set = [
                    t.hex.is_some(),
                    t.base64.is_some(),
                    t.utf8.is_some(),
                    t.bytes.is_some(),
                ]
                .iter()
                .filter(|b| **b)
                .count();
                if set == 0 {
                    return Err(de::Error::custom(
                        "byte literal map must set one of hex/base64/utf8/bytes",
                    ));
                }
                if set > 1 {
                    return Err(de::Error::custom(
                        "byte literal map must set only one of hex/base64/utf8/bytes",
                    ));
                }
                if let Some(h) = t.hex {
                    decode_hex(&h).map_err(de::Error::custom)?
                } else if let Some(b) = t.base64 {
                    base64::engine::general_purpose::STANDARD
                        .decode(b.trim())
                        .map_err(|e| de::Error::custom(format!("invalid base64: {}", e)))?
                } else if let Some(u) = t.utf8 {
                    u.into_bytes()
                } else {
                    t.bytes.unwrap()
                }
            }
        };
        Ok(Bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn de(yaml: &str) -> Result<Bytes, String> {
        serde_yaml::from_str::<Bytes>(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn plain_string_is_utf8_bytes() {
        assert_eq!(de("\"Q\\r\\n\"").unwrap().0, b"Q\r\n");
    }

    #[test]
    fn hex_decodes_with_separators() {
        assert_eq!(
            de("{ hex: \"02 51 03\" }").unwrap().0,
            vec![0x02, 0x51, 0x03]
        );
        assert_eq!(de("{ hex: \"025103\" }").unwrap().0, vec![0x02, 0x51, 0x03]);
        assert_eq!(de("{ hex: \"0x02:0x51\" }").unwrap().0, vec![0x02, 0x51]);
    }

    #[test]
    fn base64_decodes() {
        // "AlED" decodes to 0x02 0x51 0x03
        assert_eq!(
            de("{ base64: \"AlED\" }").unwrap().0,
            vec![0x02, 0x51, 0x03]
        );
    }

    #[test]
    fn utf8_tag_equals_plain() {
        assert_eq!(de("{ utf8: \"Q\\r\\n\" }").unwrap().0, b"Q\r\n");
    }

    #[test]
    fn byte_array_is_verbatim() {
        assert_eq!(de("{ bytes: [2, 81, 3] }").unwrap().0, vec![2, 81, 3]);
    }

    #[test]
    fn empty_map_rejected() {
        assert!(de("{}").is_err());
    }

    #[test]
    fn two_encodings_rejected() {
        let err = de("{ hex: \"02\", base64: \"AA==\" }").unwrap_err();
        assert!(err.contains("only one"), "{}", err);
    }

    #[test]
    fn odd_hex_rejected() {
        let err = de("{ hex: \"025\" }").unwrap_err();
        assert!(err.contains("odd"), "{}", err);
    }

    #[test]
    fn bad_hex_char_rejected() {
        let err = de("{ hex: \"02zz\" }").unwrap_err();
        assert!(err.contains("invalid hex"), "{}", err);
    }
}
