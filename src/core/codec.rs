//! Byte-level helpers shared by the log view and later the codec tools
//! (docs/design.md: `codec.rs` — "encoding conversion, garbled-text recovery, hex utils").
//! This slice: hex formatting and lossy UTF-8 decoding.

/// Formats bytes as lowercase hex pairs separated by a space: `68 69`.
pub fn hex_spaced(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Decodes bytes as UTF-8, replacing invalid sequences with the replacement
/// character (`U+FFFD`) — the log view never refuses to show data.
pub fn lossy_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_spaced_formats_pairs() {
        assert_eq!(hex_spaced(&[]), "");
        assert_eq!(hex_spaced(&[0x00]), "00");
        assert_eq!(hex_spaced(&[0x68, 0x69, 0xff]), "68 69 ff");
    }

    #[test]
    fn lossy_utf8_decodes_valid_and_replaces_invalid() {
        assert_eq!(lossy_utf8("hí".as_bytes()), "hí");
        // 0xff alone is not valid UTF-8: replaced, not an error.
        assert_eq!(lossy_utf8(&[0x68, 0xff, 0x69]), "h\u{fffd}i");
        // A split multi-byte sequence degrades per byte, never panics.
        assert_eq!(lossy_utf8(&[0xc3]), "\u{fffd}");
    }
}
