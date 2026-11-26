//! # pane-proto
//!
//! The shared wire protocol for **pane**: strongly typed messages, a
//! deterministic length-prefixed binary frame format, and the retained-mode
//! terminal state-diff engine used by both the headless host and the GPU
//! client.
//!
//! ```text
//! +----------------------+------------+--------------------------------+
//! | u32 LE payload length| u8 flags   | payload (postcard, maybe LZ4)  |
//! +----------------------+------------+--------------------------------+
//!   bit 0 of flags = payload is LZ4 block-compressed (size-prepended)
//! ```

pub mod diff;
pub mod frame;
pub mod msg;

pub use diff::{CellStyle, Color, DirtyRow, Row, RowBuilder, Screen, Span, StyleFlags, diff_rows};
pub use frame::{
    COMPRESS_THRESHOLD, FLAG_LZ4, FrameDecoder, FrameError, HEADER_LEN, MAX_FRAME_LEN, decode_payload,
    encode_frame, read_frame, write_frame,
};
pub use msg::{ClientMsg, DirEntry, HostMsg, PROTOCOL_VERSION};

/// Size of a single file-transfer chunk sent in [`HostMsg::FileChunk`].
pub const FILE_CHUNK_SIZE: usize = 64 * 1024;
