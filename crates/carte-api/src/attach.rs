//! `carte attach`: the request that opens a session, and the frames both sides send
//! once the connection is upgraded.
//!
//! A frame is a tag byte, a 4-byte big-endian length and that many bytes.

use serde::{Deserialize, Serialize};

use crate::Address;

/// The `Upgrade` header's protocol for [`crate::paths::ATTACH`].
pub const PROTOCOL: &str = "carte-attach";

/// The largest frame either side accepts.
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    pub target: Address,
    /// The process, when the address doesn't name one: the HTTP process by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    /// The client's terminal. The pty takes the smallest attached terminal's size;
    /// `None` leaves the size alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TermSize>,
    /// Send the last screen of output first.
    #[serde(default)]
    pub replay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

/// What the session is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    /// `app@stack`.
    pub instance: String,
    pub process: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The first frame from the daemon.
    Attached(Attached),
    /// Raw output of the process, from the daemon.
    Output(Vec<u8>),
    /// Bytes to type into the process, from the client; one frame per read of its
    /// terminal, written as a whole.
    Input(Vec<u8>),
    /// The client's terminal changed size.
    Resize(TermSize),
    /// The process exited, in words; the daemon closes the connection after it.
    Ended(String),
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (tag, payload) = match self {
            Frame::Attached(attached) => (0, serde_json::to_vec(attached).unwrap_or_default()),
            Frame::Output(data) => (1, data.clone()),
            Frame::Input(data) => (2, data.clone()),
            Frame::Resize(size) => (
                3,
                [size.cols.to_be_bytes(), size.rows.to_be_bytes()].concat(),
            ),
            Frame::Ended(message) => (4, message.as_bytes().to_vec()),
        };
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError(String);

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad attach frame: {}", self.0)
    }
}

impl std::error::Error for FrameError {}

/// Collects bytes from the connection and cuts them into frames.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole frame, if one has arrived. Frames with a tag this version
    /// doesn't know are skipped.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        loop {
            match self.frame()? {
                Some(Some(frame)) => return Ok(Some(frame)),
                Some(None) => {}
                None => return Ok(None),
            }
        }
    }

    /// The next whole frame, `Some(None)` for one with an unknown tag.
    fn frame(&mut self) -> Result<Option<Option<Frame>>, FrameError> {
        let Some(header) = self.buf.get(..5) else {
            return Ok(None);
        };
        let tag = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_FRAME {
            return Err(FrameError(format!("{len} bytes is too long")));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload: Vec<u8> = self.buf.drain(..5 + len).skip(5).collect();
        let frame = match tag {
            0 => Frame::Attached(
                serde_json::from_slice(&payload).map_err(|e| FrameError(e.to_string()))?,
            ),
            1 => Frame::Output(payload),
            2 => Frame::Input(payload),
            3 if payload.len() == 4 => Frame::Resize(TermSize {
                cols: u16::from_be_bytes([payload[0], payload[1]]),
                rows: u16::from_be_bytes([payload[2], payload[3]]),
            }),
            3 => return Err(FrameError(format!("a resize of {len} bytes"))),
            4 => Frame::Ended(String::from_utf8_lossy(&payload).into_owned()),
            _ => return Ok(Some(None)),
        };
        Ok(Some(Some(frame)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_any_split() {
        let frames = vec![
            Frame::Attached(Attached {
                instance: "shop@feature-x".into(),
                process: "web".into(),
            }),
            Frame::Output(b"(rdbg) ".to_vec()),
            Frame::Input(b"\x1b[A".to_vec()),
            Frame::Resize(TermSize {
                cols: 300,
                rows: 40,
            }),
            Frame::Input(Vec::new()),
            Frame::Ended("web exited with code 1".into()),
        ];
        let bytes: Vec<u8> = frames.iter().flat_map(Frame::encode).collect();
        for chunk in [1, 3, 7, bytes.len()] {
            let mut reader = FrameReader::default();
            let mut got = Vec::new();
            for piece in bytes.chunks(chunk) {
                reader.feed(piece);
                while let Some(frame) = reader.next_frame().unwrap() {
                    got.push(frame);
                }
            }
            assert_eq!(got, frames, "in chunks of {chunk}");
        }
    }

    #[test]
    fn skips_unknown_frames_and_rejects_oversized_ones() {
        let mut reader = FrameReader::default();
        reader.feed(&[9, 0, 0, 0, 2, 1, 2]);
        reader.feed(&Frame::Input(b"x".to_vec()).encode());
        assert_eq!(
            reader.next_frame().unwrap(),
            Some(Frame::Input(b"x".to_vec())),
            "a newer frame is skipped"
        );
        let mut reader = FrameReader::default();
        reader.feed(&[1, 0xff, 0, 0, 0]);
        assert!(reader.next_frame().is_err());
        let mut reader = FrameReader::default();
        reader.feed(&[3, 0, 0, 0, 1, 7]);
        assert_eq!(
            reader.next_frame().unwrap_err().to_string(),
            "bad attach frame: a resize of 1 bytes"
        );
    }
}
