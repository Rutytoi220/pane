//! Deterministic binary framing.
//!
//! Every frame is `[u32 LE payload length][u8 flags][payload]`. The payload is
//! a `postcard`-encoded message. If the encoded message is larger than
//! [`COMPRESS_THRESHOLD`] bytes it is LZ4 block-compressed (with the
//! uncompressed size prepended) and bit 0 of `flags` is set — but only if
//! compression actually makes it smaller, so incompressible data (e.g. a
//! `.tar.gz` being downloaded) is never inflated.

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

/// Size of the frame header: 4-byte length + 1-byte flags.
pub const HEADER_LEN: usize = 5;
/// Flag bit: payload is LZ4 compressed.
pub const FLAG_LZ4: u8 = 0b0000_0001;
/// Payloads strictly larger than this many bytes are candidates for compression.
pub const COMPRESS_THRESHOLD: usize = 128;
/// Upper bound on a single frame's payload, protecting against corrupt streams
/// (e.g. a shell MOTD accidentally printed onto the binary channel).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Errors produced while encoding or decoding frames.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("lz4 decompression failed: {0}")]
    Decompress(#[from] lz4_flex::block::DecompressError),
    #[error("frame payload of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit (corrupt stream?)")]
    TooLarge(usize),
    #[error("unknown frame flags {0:#04x}")]
    UnknownFlags(u8),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("stream ended in the middle of a frame")]
    Truncated,
}

/// Serialize `msg` and wrap it in a complete frame (header + payload).
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let raw = postcard::to_allocvec(msg)?;
    let (flags, payload) = if raw.len() > COMPRESS_THRESHOLD {
        let compressed = lz4_flex::compress_prepend_size(&raw);
        if compressed.len() < raw.len() { (FLAG_LZ4, compressed) } else { (0, raw) }
    } else {
        (0, raw)
    };
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a frame payload (without header) given its flags byte.
pub fn decode_payload<T: DeserializeOwned>(flags: u8, payload: &[u8]) -> Result<T, FrameError> {
    if flags & !FLAG_LZ4 != 0 {
        return Err(FrameError::UnknownFlags(flags));
    }
    if flags & FLAG_LZ4 != 0 {
        let raw = lz4_flex::decompress_size_prepended(payload)?;
        Ok(postcard::from_bytes(&raw)?)
    } else {
        Ok(postcard::from_bytes(payload)?)
    }
}

/// Encode `msg` and write it to `w`. Returns the number of bytes written.
/// Does not flush.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<usize, FrameError> {
    let frame = encode_frame(msg)?;
    w.write_all(&frame)?;
    Ok(frame.len())
}

/// Blocking read of exactly one frame from `r`.
///
/// Returns `Ok(None)` on a clean EOF at a frame boundary, and
/// `Err(FrameError::Truncated)` if the stream ends mid-frame. On success the
/// decoded message is returned along with its on-wire size in bytes.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<(T, usize)>, FrameError> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match r.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let flags = header[4];
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(e),
    })?;
    Ok(Some((decode_payload(flags, &payload)?, HEADER_LEN + len)))
}

/// Incremental, non-blocking frame decoder.
///
/// Feed it arbitrary byte slices as they arrive (split anywhere, even inside
/// the header) with [`push`](Self::push), then drain complete messages with
/// [`next_frame`](Self::next_frame).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Read cursor into `buf`; consumed bytes are compacted lazily.
    pos: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Number of buffered bytes not yet consumed by a complete frame.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Try to decode the next complete frame. Returns `Ok(None)` if more
    /// bytes are needed. On success also returns the on-wire frame size.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<(T, usize)>, FrameError> {
        let avail = &self.buf[self.pos..];
        if avail.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        let total = HEADER_LEN + len;
        if avail.len() < total {
            return Ok(None);
        }
        let flags = avail[4];
        let msg = decode_payload(flags, &avail[HEADER_LEN..total]);
        // Consume the frame even if decoding failed so callers can skip it.
        self.pos += total;
        Ok(Some((msg?, total)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{CellStyle, Color, DirtyRow, Span, StyleFlags};
    use crate::msg::{ClientMsg, DirEntry, HostMsg, PROTOCOL_VERSION};
    use std::io::Cursor;

    fn sample_diff(n_rows: u16) -> HostMsg {
        HostMsg::TermDiff {
            term_id: 7,
            screen_cols: 120,
            screen_rows: n_rows,
            cursor_row: 3,
            cursor_col: 9,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: true,
            scrollback: 0,
            rows: (0..n_rows)
                .map(|row| DirtyRow {
                    row,
                    spans: vec![
                        Span { width: 6, text: "user@h".into(), style: CellStyle::default() },
                        Span {
                            width: 40,
                            text: "~/Projects/pane $ cargo test --workspace".into(),
                            style: CellStyle {
                                fg: Color::Idx(2),
                                bg: Color::Default,
                                flags: StyleFlags::BOLD,
                            },
                        },
                    ],
                })
                .collect(),
        }
    }

    #[test]
    fn small_frame_round_trip_uncompressed() {
        let msg = ClientMsg::Hello { protocol_version: PROTOCOL_VERSION };
        let frame = encode_frame(&msg).unwrap();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - HEADER_LEN, "length prefix must equal payload size");
        assert_eq!(frame[4], 0, "tiny payloads must not be compressed");

        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let (back, wire): (ClientMsg, usize) = dec.next_frame().unwrap().unwrap();
        assert_eq!(back, msg);
        assert_eq!(wire, frame.len());
        assert_eq!(dec.pending(), 0);
    }

    #[test]
    fn encoding_is_deterministic() {
        let a = encode_frame(&sample_diff(24)).unwrap();
        let b = encode_frame(&sample_diff(24)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn large_frame_is_lz4_compressed_and_round_trips() {
        let msg = sample_diff(50);
        let raw_len = postcard::to_allocvec(&msg).unwrap().len();
        assert!(raw_len > COMPRESS_THRESHOLD);

        let frame = encode_frame(&msg).unwrap();
        assert_eq!(frame[4] & FLAG_LZ4, FLAG_LZ4, "large repetitive payload must be compressed");
        assert!(frame.len() < raw_len, "compressed frame ({}) should beat raw ({raw_len})", frame.len());

        let (back, _): (HostMsg, usize) = read_frame(&mut Cursor::new(&frame)).unwrap().unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn lz4_round_trip_of_file_chunk() {
        let data: Vec<u8> = b"fn main() { println!(\"hello pane\"); }\n".repeat(1800);
        assert!(data.len() > crate::FILE_CHUNK_SIZE);
        let msg = HostMsg::FileChunk {
            req_id: 42,
            offset: 65536,
            total_size: 1 << 20,
            data: data[..crate::FILE_CHUNK_SIZE].to_vec(),
            eof: false,
        };
        let frame = encode_frame(&msg).unwrap();
        assert_eq!(frame[4], FLAG_LZ4);
        assert!(frame.len() < crate::FILE_CHUNK_SIZE / 4);
        let (back, _): (HostMsg, usize) = read_frame(&mut Cursor::new(&frame)).unwrap().unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn incompressible_payload_is_sent_raw() {
        // xorshift noise: LZ4 cannot shrink this, so the flag must stay clear.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let msg = HostMsg::FileChunk { req_id: 1, offset: 0, total_size: 4096, data, eof: true };
        let frame = encode_frame(&msg).unwrap();
        assert_eq!(frame[4], 0);
        let (back, _): (HostMsg, usize) = read_frame(&mut Cursor::new(&frame)).unwrap().unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn split_buffer_partial_reads_byte_by_byte() {
        let msgs = vec![
            HostMsg::Pong { timestamp_micros: 123_456_789 },
            sample_diff(40),
            HostMsg::DirListing {
                req_id: 9,
                path: "/home/dev".into(),
                entries: vec![DirEntry {
                    name: "Cargo.toml".into(),
                    is_dir: false,
                    is_symlink: false,
                    size: 512,
                    modified_unix: Some(1_700_000_000),
                }],
            },
        ];
        let stream: Vec<u8> = msgs.iter().flat_map(|m| encode_frame(m).unwrap()).collect();

        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        for byte in &stream {
            dec.push(std::slice::from_ref(byte));
            while let Some((m, _)) = dec.next_frame::<HostMsg>().unwrap() {
                out.push(m);
            }
        }
        assert_eq!(out, msgs);
        assert_eq!(dec.pending(), 0);
    }

    #[test]
    fn split_buffer_at_every_boundary() {
        let a = encode_frame(&sample_diff(30)).unwrap();
        let b = encode_frame(&HostMsg::Pong { timestamp_micros: 1 }).unwrap();
        let stream = [a.clone(), b.clone()].concat();
        for split in 0..=stream.len() {
            let mut dec = FrameDecoder::new();
            dec.push(&stream[..split]);
            let mut got = Vec::new();
            while let Some((m, _)) = dec.next_frame::<HostMsg>().unwrap() {
                got.push(m);
            }
            // Only frames fully contained in the first half may be decoded.
            let expect_first = if split >= a.len() { 1 } else { 0 } + if split >= stream.len() { 1 } else { 0 };
            assert_eq!(got.len(), expect_first, "split at {split}");
            dec.push(&stream[split..]);
            while let Some((m, _)) = dec.next_frame::<HostMsg>().unwrap() {
                got.push(m);
            }
            assert_eq!(got, vec![sample_diff(30), HostMsg::Pong { timestamp_micros: 1 }]);
        }
    }

    #[test]
    fn blocking_reader_handles_multiple_frames_and_eof() {
        let mut stream = Vec::new();
        for i in 0..5u64 {
            write_frame(&mut stream, &ClientMsg::Ping { timestamp_micros: i }).unwrap();
        }
        let mut cur = Cursor::new(stream);
        for i in 0..5u64 {
            let (m, _): (ClientMsg, usize) = read_frame(&mut cur).unwrap().unwrap();
            assert_eq!(m, ClientMsg::Ping { timestamp_micros: i });
        }
        assert!(read_frame::<_, ClientMsg>(&mut cur).unwrap().is_none(), "clean EOF");
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let frame = encode_frame(&sample_diff(10)).unwrap();
        for cut in [2, HEADER_LEN, frame.len() - 1] {
            let err = read_frame::<_, HostMsg>(&mut Cursor::new(&frame[..cut])).unwrap_err();
            assert!(matches!(err, FrameError::Truncated), "cut at {cut}: {err:?}");
        }
    }

    #[test]
    fn oversized_and_garbage_frames_are_rejected() {
        // Something like "Welcome to Ubuntu" printed by a noisy .bashrc.
        let mut dec = FrameDecoder::new();
        dec.push(b"Welcome to Ubuntu 24.04 LTS\n");
        assert!(matches!(dec.next_frame::<HostMsg>(), Err(FrameError::TooLarge(_))));

        let mut bad_flags = encode_frame(&HostMsg::Pong { timestamp_micros: 5 }).unwrap();
        bad_flags[4] = 0x80;
        assert!(matches!(
            read_frame::<_, HostMsg>(&mut Cursor::new(&bad_flags)),
            Err(FrameError::UnknownFlags(0x80))
        ));
    }
}
