// Byte-stream framing.
//
// The reader thread receives raw chunks from the transport and must cut
// them into discrete frames before matching. The historical behavior was
// a single hardcoded strategy: split on `\n`, terminator included. This
// module generalizes that into four strategies, selected per port:
//
//   - Delimiter:     split on a configurable byte sequence (default `\n`)
//   - Fixed:         every frame is exactly N bytes
//   - LengthPrefixed: a header field declares the payload length
//   - IdleTimeout:   no delimiter; flush accumulated bytes after a quiet gap
//
// `FramingSpec` is immutable and shareable (`Arc`). The mutable framing
// buffer lives in the reader thread and is passed in to `push`/`on_idle`,
// which keeps the spec swappable on reload without disturbing in-flight
// bytes, and makes framing unit-testable without a PTY.

use std::time::Duration;

use serde::Deserialize;

use crate::bytes::Bytes;

/// Mirrors the historical `MAX_LINE_BYTES`: the largest partial frame the
/// reader buffers before force-flushing it as one (unmatched) frame so a
/// stream with no frame boundary can't grow memory without bound.
const DEFAULT_MAX_BUFFER: usize = 4096;

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    #[default]
    Big,
    Little,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LengthIncludes {
    /// The length field counts only the payload bytes.
    #[default]
    Payload,
    /// The length field counts the entire frame (header + payload + trailer).
    Frame,
}

/// YAML config for a port's framing. `type` selects the variant.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FramingConfig {
    Delimiter {
        delimiter: Bytes,
        #[serde(default = "default_true")]
        include_delimiter: bool,
    },
    Fixed {
        length: usize,
    },
    LengthPrefixed {
        #[serde(default)]
        header_size: usize,
        length_offset: usize,
        length_size: usize,
        #[serde(default)]
        length_endian: Endian,
        #[serde(default)]
        length_includes: LengthIncludes,
        #[serde(default)]
        trailer_size: usize,
    },
    IdleTimeout {
        quiet_ms: u64,
    },
}

fn default_true() -> bool {
    true
}

impl FramingConfig {
    /// Reject nonsensical parameter combinations at config-load time.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            FramingConfig::Delimiter { delimiter, .. } => {
                if delimiter.0.is_empty() {
                    return Err("framing delimiter must be at least one byte".into());
                }
            }
            FramingConfig::Fixed { length } => {
                if *length == 0 {
                    return Err("framing fixed length must be greater than zero".into());
                }
            }
            FramingConfig::LengthPrefixed {
                length_size,
                length_offset,
                ..
            } => {
                if !matches!(length_size, 1 | 2 | 4) {
                    return Err(format!(
                        "framing length_size must be 1, 2, or 4 (got {})",
                        length_size
                    ));
                }
                let _ = length_offset;
            }
            FramingConfig::IdleTimeout { quiet_ms } => {
                if *quiet_ms == 0 {
                    return Err("framing quiet_ms must be greater than zero".into());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum Strategy {
    Delimiter {
        delim: Vec<u8>,
        include: bool,
    },
    Fixed {
        len: usize,
    },
    LengthPrefixed {
        header_size: usize,
        length_offset: usize,
        length_size: usize,
        endian: Endian,
        includes: LengthIncludes,
        trailer_size: usize,
    },
    /// Accumulate everything; frames are emitted only by `on_idle`.
    IdleTimeout,
}

/// Immutable, shareable framing strategy. The reader thread owns the byte
/// buffer and passes it in; this type holds no mutable state.
#[derive(Debug, Clone)]
pub struct FramingSpec {
    strategy: Strategy,
    max_buffer: usize,
    idle: Option<Duration>,
}

impl Default for FramingSpec {
    /// The legacy default: newline-delimited, terminator included.
    fn default() -> Self {
        FramingSpec {
            strategy: Strategy::Delimiter {
                delim: vec![b'\n'],
                include: true,
            },
            max_buffer: DEFAULT_MAX_BUFFER,
            idle: None,
        }
    }
}

impl FramingSpec {
    pub fn from_config(cfg: Option<&FramingConfig>) -> Self {
        let Some(cfg) = cfg else {
            return FramingSpec::default();
        };
        match cfg {
            FramingConfig::Delimiter {
                delimiter,
                include_delimiter,
            } => FramingSpec {
                strategy: Strategy::Delimiter {
                    delim: delimiter.0.clone(),
                    include: *include_delimiter,
                },
                max_buffer: DEFAULT_MAX_BUFFER,
                idle: None,
            },
            FramingConfig::Fixed { length } => FramingSpec {
                strategy: Strategy::Fixed { len: *length },
                max_buffer: DEFAULT_MAX_BUFFER.max(*length),
                idle: None,
            },
            FramingConfig::LengthPrefixed {
                header_size,
                length_offset,
                length_size,
                length_endian,
                length_includes,
                trailer_size,
            } => FramingSpec {
                strategy: Strategy::LengthPrefixed {
                    header_size: *header_size,
                    length_offset: *length_offset,
                    length_size: *length_size,
                    endian: *length_endian,
                    includes: *length_includes,
                    trailer_size: *trailer_size,
                },
                max_buffer: DEFAULT_MAX_BUFFER,
                idle: None,
            },
            FramingConfig::IdleTimeout { quiet_ms } => FramingSpec {
                strategy: Strategy::IdleTimeout,
                max_buffer: DEFAULT_MAX_BUFFER,
                idle: Some(Duration::from_millis(*quiet_ms)),
            },
        }
    }

    /// The quiet period after which `on_idle` should be called, if this
    /// strategy is idle-driven. `None` means block on read indefinitely.
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle
    }

    /// Feed a freshly-read chunk; append completed frames to `out`.
    pub fn push(&self, buf: &mut Vec<u8>, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        buf.extend_from_slice(chunk);
        self.drain(buf, out);
        // Backstop: a stream that never completes a frame must not grow
        // forever. Flush the whole buffer as one frame.
        if buf.len() > self.max_buffer {
            out.push(std::mem::take(buf));
        }
    }

    /// Called after a quiet gap. For idle-timeout framing this flushes the
    /// accumulated buffer as a single frame; other strategies do nothing.
    pub fn on_idle(&self, buf: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
        if matches!(self.strategy, Strategy::IdleTimeout) && !buf.is_empty() {
            out.push(std::mem::take(buf));
        }
    }

    fn drain(&self, buf: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
        match &self.strategy {
            Strategy::Delimiter { delim, include } => {
                while let Some(pos) = find_subslice(buf, delim) {
                    let end = pos + delim.len();
                    let take = if *include { end } else { pos };
                    let frame: Vec<u8> = buf[..take].to_vec();
                    buf.drain(..end);
                    out.push(frame);
                }
            }
            Strategy::Fixed { len } => {
                while buf.len() >= *len {
                    let frame: Vec<u8> = buf[..*len].to_vec();
                    buf.drain(..*len);
                    out.push(frame);
                }
            }
            Strategy::LengthPrefixed {
                header_size,
                length_offset,
                length_size,
                endian,
                includes,
                trailer_size,
            } => loop {
                // Need enough bytes to read the length field.
                let need_for_len = length_offset + length_size;
                if buf.len() < need_for_len {
                    break;
                }
                let l = read_uint(&buf[*length_offset..need_for_len], *endian);
                let total = match includes {
                    LengthIncludes::Payload => header_size + l as usize + trailer_size,
                    LengthIncludes::Frame => l as usize,
                };
                // Guard against a zero/too-small declared length that would
                // stall or loop forever: require progress.
                if total == 0 || total < need_for_len {
                    // Malformed length — flush whatever we have, unmatched.
                    out.push(std::mem::take(buf));
                    break;
                }
                if buf.len() < total {
                    break;
                }
                let frame: Vec<u8> = buf[..total].to_vec();
                buf.drain(..total);
                out.push(frame);
            },
            Strategy::IdleTimeout => {
                // Frames emitted only by on_idle; accumulate here.
            }
        }
    }
}

/// Read a 1/2/4-byte unsigned integer with the given endianness.
fn read_uint(bytes: &[u8], endian: Endian) -> u64 {
    let mut v: u64 = 0;
    match endian {
        Endian::Big => {
            for &b in bytes {
                v = (v << 8) | b as u64;
            }
        }
        Endian::Little => {
            for (i, &b) in bytes.iter().enumerate() {
                v |= (b as u64) << (8 * i);
            }
        }
    }
    v
}

/// First index of `needle` within `haystack`, or None.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(spec: &FramingSpec, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut buf = Vec::new();
        let mut out = Vec::new();
        for c in chunks {
            spec.push(&mut buf, c, &mut out);
        }
        out
    }

    #[test]
    fn default_is_newline_delimited_inclusive() {
        let spec = FramingSpec::default();
        let got = frames(&spec, &[b"AB\nCD\n"]);
        assert_eq!(got, vec![b"AB\n".to_vec(), b"CD\n".to_vec()]);
    }

    #[test]
    fn delimiter_split_across_chunks() {
        let spec = FramingSpec::from_config(Some(&FramingConfig::Delimiter {
            delimiter: vec![0x0D, 0x0A].into(),
            include_delimiter: true,
        }));
        // The CRLF arrives split across two reads.
        let got = frames(&spec, &[b"hello\r", b"\nworld\r\n"]);
        assert_eq!(got, vec![b"hello\r\n".to_vec(), b"world\r\n".to_vec()]);
    }

    #[test]
    fn delimiter_exclusive_strips_terminator() {
        let spec = FramingSpec::from_config(Some(&FramingConfig::Delimiter {
            delimiter: vec![b'\n'].into(),
            include_delimiter: false,
        }));
        let got = frames(&spec, &[b"AB\nCD\n"]);
        assert_eq!(got, vec![b"AB".to_vec(), b"CD".to_vec()]);
    }

    #[test]
    fn fixed_length_frames() {
        let spec = FramingSpec::from_config(Some(&FramingConfig::Fixed { length: 3 }));
        let got = frames(&spec, &[b"AABB", b"CCDD"]);
        assert_eq!(got, vec![b"AAB".to_vec(), b"BCC".to_vec()]);
        // One byte (D) remains buffered, not emitted.
    }

    #[test]
    fn length_prefixed_payload_big_endian() {
        // Layout: [STX][len:1][payload...][checksum:1], len counts payload.
        let spec = FramingSpec::from_config(Some(&FramingConfig::LengthPrefixed {
            header_size: 2,
            length_offset: 1,
            length_size: 1,
            length_endian: Endian::Big,
            length_includes: LengthIncludes::Payload,
            trailer_size: 1,
        }));
        // STX=0x02, len=3, payload=AAA, checksum=0x99 -> total 2+3+1 = 6
        let got = frames(&spec, &[&[0x02, 0x03, b'A', b'A'], &[b'A', 0x99]]);
        assert_eq!(got, vec![vec![0x02, 0x03, b'A', b'A', b'A', 0x99]]);
    }

    #[test]
    fn length_prefixed_frame_includes_two_byte_le() {
        // length field counts the whole frame, 2-byte little-endian at offset 0.
        let spec = FramingSpec::from_config(Some(&FramingConfig::LengthPrefixed {
            header_size: 0,
            length_offset: 0,
            length_size: 2,
            length_endian: Endian::Little,
            length_includes: LengthIncludes::Frame,
            trailer_size: 0,
        }));
        // total = 5 (0x05 0x00), then 3 more bytes
        let got = frames(&spec, &[&[0x05, 0x00, 0x11, 0x22, 0x33], &[0xFF]]);
        assert_eq!(got, vec![vec![0x05, 0x00, 0x11, 0x22, 0x33]]);
    }

    #[test]
    fn idle_timeout_accumulates_then_flushes() {
        let spec = FramingSpec::from_config(Some(&FramingConfig::IdleTimeout { quiet_ms: 50 }));
        assert_eq!(spec.idle_timeout(), Some(Duration::from_millis(50)));
        let mut buf = Vec::new();
        let mut out = Vec::new();
        spec.push(&mut buf, &[0x01, 0x02], &mut out);
        spec.push(&mut buf, &[0x03], &mut out);
        assert!(out.is_empty(), "no frames until idle");
        spec.on_idle(&mut buf, &mut out);
        assert_eq!(out, vec![vec![0x01, 0x02, 0x03]]);
        // Second idle with empty buffer yields nothing.
        spec.on_idle(&mut buf, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn oversize_buffer_force_flushes() {
        // No delimiter ever arrives; buffer should flush at the backstop.
        let spec = FramingSpec::default();
        let big = vec![b'x'; DEFAULT_MAX_BUFFER + 10];
        let mut buf = Vec::new();
        let mut out = Vec::new();
        spec.push(&mut buf, &big, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), DEFAULT_MAX_BUFFER + 10);
    }
}
