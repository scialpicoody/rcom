//! The demo framed protocol (issue #24): the wire format the bundled decoder
//! script parses, the generator emits and the tests share.
//!
//! Frame layout, 7 bytes:
//!
//! ```text
//! AA 55 | len u8 | ch u8 | value i16 big-endian | checksum u8
//! ```
//!
//! `len` counts the payload bytes after it (`ch` + the two value bytes = 3);
//! `checksum` is `(len + ch + hi + lo) & 0xFF`. The Rust side here only ever
//! *builds* frames (the demo generator) and reference-parses them (tests, demo
//! self-check) — the actual decoding on the wire is the bundled Lua decoder's
//! job, this module is its byte-level contract.

/// The frame head every frame starts with.
pub const FRAME_HEAD: [u8; 2] = [0xAA, 0x55];

/// The payload length the format fixes: channel byte + i16 value.
pub const PAYLOAD_LEN: u8 = 3;

/// The checksum: one byte, the wrapping sum of the payload bytes.
pub fn checksum(payload: &[u8]) -> u8 {
    payload.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

/// Builds one well-formed frame for `channel` carrying `value`.
pub fn build_frame(channel: u8, value: i16) -> Vec<u8> {
    let [hi, lo] = value.to_be_bytes();
    let payload = [PAYLOAD_LEN, channel, hi, lo];
    let mut frame = Vec::with_capacity(7);
    frame.extend_from_slice(&FRAME_HEAD);
    frame.extend_from_slice(&payload);
    frame.push(checksum(&payload));
    frame
}

/// Builds one frame that is deliberately *corrupt* (bad checksum) — the
/// decoder must skip it and count it, never crash on it.
pub fn build_corrupt_frame(channel: u8, value: i16) -> Vec<u8> {
    let mut frame = build_frame(channel, value);
    let last = frame.len() - 1;
    frame[last] = frame[last].wrapping_add(1);
    frame
}

/// Builds a demo chunk: one frame each for `0..channels`, plus `corrupt_every`
/// logic is the caller's (the generator decides when to sneak one in).
pub fn build_chunk(channels: u8, value_of: impl Fn(u8) -> i16) -> Vec<u8> {
    let mut chunk = Vec::new();
    for channel in 0..channels {
        chunk.extend_from_slice(&build_frame(channel, value_of(channel)));
    }
    chunk
}

/// What a reference parse found in one feed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParseStats {
    pub frames: Vec<(u8, i16)>,
    pub skipped: usize,
}

/// A streaming reference parser with the same chunk-boundary semantics the Lua
/// decoder implements: bytes buffer until a whole frame is available; anything
/// that fails header/length/checksum validation is skipped one byte at a time
/// (resync inside the stream).
#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    /// Feeds a received chunk, returning every complete frame parsed from it
    /// (including frames completed by this chunk's tail).
    pub fn feed(&mut self, bytes: &[u8]) -> ParseStats {
        self.buf.extend_from_slice(bytes);
        let mut stats = ParseStats::default();
        loop {
            match self.parse_one() {
                Some(Ok(frame)) => stats.frames.push(frame),
                Some(Err(skipped)) => stats.skipped += skipped,
                None => break,
            }
        }
        stats
    }

    /// Tries to parse one frame off the buffer front.
    /// `Some(Ok((ch, value)))` — a frame consumed; `Some(Err(n))` — `n`
    /// garbage bytes consumed while resyncing; `None` — need more bytes.
    fn parse_one(&mut self) -> Option<Result<(u8, i16), usize>> {
        const FRAME_LEN: usize = 7;
        if self.buf.len() < FRAME_LEN {
            return None;
        }
        let head_pos = self.buf.windows(2).position(|window| window == FRAME_HEAD);
        let head_pos = match head_pos {
            Some(pos) => pos,
            None => {
                // No head: keep the last byte (it may be 0xAA followed by the
                // next chunk's 0x55) and drop the rest.
                let keep = self.buf.split_off(self.buf.len() - 1);
                let skipped = self.buf.len();
                self.buf = keep;
                return Some(Err(skipped));
            }
        };
        if head_pos > 0 {
            let keep = self.buf.split_off(head_pos);
            let skipped = self.buf.len();
            self.buf = keep;
            return Some(Err(skipped));
        }
        // Wire layout: head(2) | len u8 | len payload bytes | checksum u8 —
        // the checksum covers the len byte plus the payload after it.
        let len = self.buf[2] as usize;
        if len != PAYLOAD_LEN as usize {
            // A wrong length cannot be trusted to frame the stream: drop the
            // head and resync.
            self.buf.drain(..2);
            return Some(Err(2));
        }
        if self.buf.len() < FRAME_LEN {
            return None; // frame not complete yet
        }
        let payload = &self.buf[2..2 + len + 1]; // len byte + the counted bytes
        let expected = checksum(payload);
        if self.buf[2 + len + 1] != expected {
            self.buf.drain(..2);
            return Some(Err(2));
        }
        let channel = self.buf[3];
        let value = i16::from_be_bytes([self.buf[4], self.buf[5]]);
        self.buf.drain(..FRAME_LEN);
        Some(Ok((channel, value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A built frame is exactly 7 bytes, starts with the head and carries the
    /// value big-endian.
    #[test]
    fn build_frame_layout() {
        let frame = build_frame(2, -300);
        assert_eq!(frame, vec![0xAA, 0x55, 0x03, 0x02, 0xFE, 0xD4, checksum(&[0x03, 0x02, 0xFE, 0xD4])]);
        assert_eq!(frame.len(), 7);
        assert_eq!(i16::from_be_bytes([frame[4], frame[5]]), -300);
    }

    /// The reference parser round-trips every built frame, across arbitrary
    /// chunk boundaries (one byte at a time included).
    #[test]
    fn parser_round_trips_frames_across_chunk_boundaries() {
        let mut expected = Vec::new();
        let mut wire = Vec::new();
        for i in 0..8u8 {
            let value = (i as i16 + 1) * 100;
            expected.push((i, value));
            wire.extend_from_slice(&build_frame(i, value));
        }

        // Whole chunk in one feed.
        let mut parser = FrameParser::default();
        let stats = parser.feed(&wire);
        assert_eq!(stats.frames, expected);
        assert_eq!(stats.skipped, 0);

        // One byte at a time.
        let mut parser = FrameParser::default();
        let mut all = ParseStats::default();
        for byte in &wire {
            let stats = parser.feed(std::slice::from_ref(byte));
            all.frames.extend(stats.frames);
            all.skipped += stats.skipped;
        }
        assert_eq!(all.frames, expected);
        assert_eq!(all.skipped, 0);
    }

    /// Corrupt frames are skipped (checksum reject, wrong length, garbage
    /// between frames) and the stream resyncs to the next good frame.
    #[test]
    fn parser_skips_corrupt_frames_and_resyncs() {
        let mut parser = FrameParser::default();
        let mut wire = build_frame(0, 111);
        wire.extend_from_slice(&build_corrupt_frame(1, 222));
        wire.extend_from_slice(&build_frame(2, 333));
        wire.extend_from_slice(&[0x01, 0x02, 0x03]); // garbage without a head
        wire.extend_from_slice(&build_frame(3, 444));

        let stats = parser.feed(&wire);
        assert_eq!(stats.frames, vec![(0, 111), (2, 333), (3, 444)]);
        assert!(stats.skipped >= 2, "corrupt frame + garbage must count as skipped: {stats:?}");
    }

    /// Partial frames stay buffered until the tail arrives (the demo's
    /// chunk-boundary case).
    #[test]
    fn parser_buffers_partial_frames() {
        let mut parser = FrameParser::default();
        let frame = build_frame(7, -1);
        let (head, tail) = frame.split_at(5);
        assert!(parser.feed(head).frames.is_empty());
        let stats = parser.feed(tail);
        assert_eq!(stats.frames, vec![(7, -1)]);
    }
}
