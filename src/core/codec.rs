//! Byte-level helpers shared by the log view and later the codec tools
//! (docs/design.md: `codec.rs` — "encoding conversion, garbled-text recovery, hex utils").
//! This slice: hex formatting/parsing and the text encodings (via `encoding_rs`)
//! that the UI applies to both display decode and send encode.

use encoding_rs::Encoding;

/// The text encodings rcom offers (ticket #17: UTF-8 default, plus the CJK set
/// llcom covers). Each value maps 1:1 to an `encoding_rs` encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Gbk,
    Gb18030,
    Big5,
    ShiftJis,
}

impl TextEncoding {
    /// Every encoding in selector order; indexes here are the UI selector's indexes.
    pub fn all() -> &'static [TextEncoding] {
        &[Self::Utf8, Self::Gbk, Self::Gb18030, Self::Big5, Self::ShiftJis]
    }

    /// The stable id (also the llcom-parity name of the encoding).
    pub fn id(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Gbk => "gbk",
            Self::Gb18030 => "gb18030",
            Self::Big5 => "big5",
            Self::ShiftJis => "shift_jis",
        }
    }

    /// The selector label. Encoding names are proper nouns — the same in every
    /// locale, so this does not go through the i18n dictionaries.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gbk => "GBK",
            Self::Gb18030 => "GB18030",
            Self::Big5 => "Big5",
            Self::ShiftJis => "Shift_JIS",
        }
    }

    /// Looks an encoding up by [`id`](TextEncoding::id); `None` when unknown.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::all().iter().copied().find(|encoding| encoding.id() == id)
    }

    fn encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8 => encoding_rs::UTF_8,
            Self::Gbk => encoding_rs::GBK,
            Self::Gb18030 => encoding_rs::GB18030,
            Self::Big5 => encoding_rs::BIG5,
            Self::ShiftJis => encoding_rs::SHIFT_JIS,
        }
    }

    /// Decodes bytes with this encoding, replacing invalid sequences with the
    /// replacement character (`U+FFFD`) — the log view never refuses to show data.
    /// A byte string may decode differently per encoding (a GBK log re-renders
    /// when the selector changes); this is the single decode path.
    pub fn decode(self, bytes: &[u8]) -> String {
        self.encoding().decode(bytes).0.into_owned()
    }

    /// Whether decoding hit invalid sequences for this encoding (the garbled-row
    /// flag of the contract's `LogEvent`).
    pub fn had_errors(self, bytes: &[u8]) -> bool {
        self.encoding().decode(bytes).2
    }

    /// Encodes text with this encoding for the wire. Characters the encoding
    /// cannot represent become XML numeric entities (`&#1234;`) — encoding_rs's
    /// documented fallback, so a send always produces bytes, never an error.
    pub fn encode(self, text: &str) -> Vec<u8> {
        self.encoding().encode(text).0.into_owned()
    }
}

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

/// Parses hex text (`"0F 0A"`, `"0f0a"`, with optional `0x` prefixes) into bytes.
/// Whitespace and colons separate pairs; the digit count must be even.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, ParseHexError> {
    let mut nibbles = Vec::with_capacity(text.len());
    for word in text.split(|c: char| c.is_whitespace() || c == ':' || c == ',') {
        let word = word.strip_prefix("0x").or_else(|| word.strip_prefix("0X")).unwrap_or(word);
        for nibble in word.bytes() {
            let digit = match nibble {
                b'0'..=b'9' => nibble - b'0',
                b'a'..=b'f' => nibble - b'a' + 10,
                b'A'..=b'F' => nibble - b'A' + 10,
                _ => return Err(ParseHexError(nibble as char)),
            };
            nibbles.push(digit);
        }
    }
    if nibbles.len() % 2 != 0 {
        return Err(ParseHexError(' '));
    }
    Ok(nibbles
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect())
}

/// A hex parse failure: the offending character, or a space for "odd digit count".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseHexError(pub char);

impl std::fmt::Display for ParseHexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == ' ' {
            write!(f, "odd number of hex digits")
        } else {
            write!(f, "invalid hex character {:?}", self.0)
        }
    }
}

impl std::error::Error for ParseHexError {}

/// Decodes bytes as UTF-8, replacing invalid sequences with the replacement
/// character (`U+FFFD`) — the log view never refuses to show data.
/// ([`TextEncoding::Utf8::decode`](TextEncoding::Utf8) is the general form.)
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

    /// The GBK bytes of 你好 decode back to 你好 — the encoding demo's core proof.
    #[test]
    fn gbk_round_trips_chinese_text() {
        let bytes = TextEncoding::Gbk.encode("你好");
        assert_eq!(bytes, vec![0xc4, 0xe3, 0xba, 0xc3]);
        assert_eq!(TextEncoding::Gbk.decode(&bytes), "你好");
        assert!(!TextEncoding::Gbk.had_errors(&bytes));
    }

    /// The same GBK bytes read as UTF-8 are lossy garbage — what the log shows
    /// before the user switches the selector.
    #[test]
    fn gbk_bytes_are_garbled_under_utf8() {
        let bytes = [0xc4, 0xe3, 0xba, 0xc3];
        assert!(TextEncoding::Utf8.had_errors(&bytes));
        assert!(TextEncoding::Utf8.decode(&bytes).contains('\u{fffd}'));
        assert_eq!(TextEncoding::Gbk.decode(&bytes), "你好");
    }

    /// Every supported encoding round-trips its native sample; ids are stable.
    #[test]
    fn every_encoding_round_trips_and_has_stable_ids() {
        let samples = [
            (TextEncoding::Utf8, "hí 你好 ｺﾝﾆﾁﾊ"),
            (TextEncoding::Gbk, "你好，串口"),
            (TextEncoding::Gb18030, "你好，串口　€"),
            (TextEncoding::Big5, "你好，串口"),
            (TextEncoding::ShiftJis, "こんにちは、シリアル"),
        ];
        for (encoding, sample) in samples {
            assert_eq!(
                encoding.decode(&encoding.encode(sample)),
                sample,
                "{} round trip",
                encoding.id()
            );
        }
        let ids = ["utf-8", "gbk", "gb18030", "big5", "shift_jis"];
        for (encoding, id) in TextEncoding::all().iter().zip(ids) {
            assert_eq!(encoding.id(), id);
            assert_eq!(TextEncoding::from_id(id), Some(*encoding));
        }
        assert_eq!(TextEncoding::from_id("klingon"), None);
    }

    /// Shift_JIS decodes kana from its half-width katakana bytes.
    #[test]
    fn shift_jis_decodes_kana() {
        // "ｺﾝ" in half-width katakana: A5 B3 A5 F3 is full-width; half-width is single bytes.
        let bytes = TextEncoding::ShiftJis.encode("ｺﾝﾆﾁﾊ");
        assert_eq!(TextEncoding::ShiftJis.decode(&bytes), "ｺﾝﾆﾁﾊ");
    }

    #[test]
    fn parse_hex_accepts_spaces_cases_and_0x() {
        assert_eq!(parse_hex("0F 0A"), Ok(vec![0x0f, 0x0a]));
        assert_eq!(parse_hex("0f0a"), Ok(vec![0x0f, 0x0a]));
        assert_eq!(parse_hex("0x0F, 0x0A"), Ok(vec![0x0f, 0x0a]));
        assert_eq!(parse_hex("0F:0A"), Ok(vec![0x0f, 0x0a]));
        assert_eq!(parse_hex(""), Ok(vec![]));
        assert_eq!(parse_hex("0F 0"), Err(ParseHexError(' ')));
        assert_eq!(parse_hex("0G"), Err(ParseHexError('G')));
    }
}
