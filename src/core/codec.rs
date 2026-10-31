//! Byte-level helpers shared by the log view and the codec tools page (issue #22)
//! (docs/design.md: `codec.rs` — "encoding conversion, garbled-text recovery, hex utils").
//!
//! - Hex formatting/parsing and the text encodings (via `encoding_rs`) that the UI
//!   applies to both display decode and send encode.
//! - The conversion toolbox ([`encode_text`], [`transcode`], [`hex_encode`],
//!   [`base64_*`](base64_encode), [`url_*`](url_encode)) — pure, byte-faithful
//!   functions over the supported charsets.
//! - The garbled-text recovery ([`recover_garbled`], llcom's EncodingFixPage
//!   parity): the scored candidates for the common mis-decodes.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD_INDIFFERENT};
use base64::Engine;
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

// ---- The conversion toolbox (issue #22) -------------------------------------

/// The outcome of encoding text into a (possibly legacy) charset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOutcome {
    /// The wire bytes. Characters the encoding cannot represent went out as XML
    /// numeric entities (`&#1234;`) — encoding_rs's documented fallback, so a
    /// conversion always produces bytes, never an error.
    pub bytes: Vec<u8>,
    /// The characters that were not representable (one probe each; empty unless
    /// encoding_rs reported errors).
    pub unencodable: Vec<char>,
}

/// Encodes text into `to`'s byte form, byte-faithfully. [`TextEncoding::encode`]
/// is the send path (bytes only); this is the conversion path — it also reports
/// *what* was un-encodable so the codec page can surface it.
pub fn encode_text(text: &str, to: TextEncoding) -> EncodeOutcome {
    let encoding = to.encoding();
    let (bytes, _, had_errors) = encoding.encode(text);
    let mut unencodable = Vec::new();
    if had_errors {
        // One probe per char: a char is un-encodable iff encoding it alone errs.
        for ch in text.chars() {
            let (_, _, failed) = encoding.encode(ch.to_string().as_str());
            if failed {
                unencodable.push(ch);
            }
        }
    }
    EncodeOutcome {
        bytes: bytes.into_owned(),
        unencodable,
    }
}

/// The outcome of a charset-to-charset transcode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeOutcome {
    /// The bytes decoded with `from` (invalid sequences → `U+FFFD`).
    pub text: String,
    /// The re-encoded bytes in `to` — the byte-faithful conversion result.
    pub bytes: Vec<u8>,
    /// Whether the `from` decode hit invalid sequences (the input was not really
    /// `from`-encoded, or arrived truncated).
    pub decode_replacements: bool,
    /// Characters the `to` encoding could not represent (see [`EncodeOutcome`]).
    pub unencodable: Vec<char>,
}

/// Transcodes raw bytes from charset `from` to charset `to`, byte-faithfully:
/// decode with `from` to a Rust string, re-encode with `to`. This is the codec
/// page's Convert A→B core (the design's "encoding conversion tools").
pub fn transcode(bytes: &[u8], from: TextEncoding, to: TextEncoding) -> TranscodeOutcome {
    let text = from.decode(bytes);
    let decode_replacements = from.had_errors(bytes);
    let EncodeOutcome { bytes, unencodable } = encode_text(&text, to);
    TranscodeOutcome {
        text,
        bytes,
        decode_replacements,
        unencodable,
    }
}

/// Formats bytes as the uniform hex encoding — lowercase spaced pairs (`68 69`),
/// the exact inverse of [`parse_hex`]. ([`hex_spaced`] predates it; this is the
/// encode-side name the toolbox pairs with the decoder.)
pub fn hex_encode(bytes: &[u8]) -> String {
    hex_spaced(bytes)
}

/// A base64 decode failure (alphabet, padding or trailing bits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64Error(pub String);

impl std::fmt::Display for Base64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid base64: {}", self.0)
    }
}

impl std::error::Error for Base64Error {}

/// Encodes bytes as standard base64 (padded, RFC 4648 alphabet).
///
/// Crate choice: the tiny `base64` crate (0.23) — already in the dependency
/// graph (lockfile) and in the registry cache, so it adds no new downloads.
/// A hand-rolled 30-liner would not bring the `NO_PAD_INDIFFERENT` decode
/// tolerance the paste-heavy UI needs.
pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decodes standard base64. Padding is tolerated either way (the padded and
/// pad-indifferent engines are tried in turn); whitespace is ignored.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| STANDARD_NO_PAD_INDIFFERENT.decode(compact.as_bytes()))
        .map_err(|error| Base64Error(error.to_string()))
}

/// A URL (percent-encoding) decode failure: the offending character — a `%` not
/// followed by two hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlDecodeError(pub char);

impl std::fmt::Display for UrlDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            ' ' => write!(f, "percent escape ended mid-sequence"),
            other => write!(f, "invalid percent escape at {:?}", other),
        }
    }
}

impl std::error::Error for UrlDecodeError {}

/// Percent-encodes the UTF-8 bytes of `text` (RFC 3986): unreserved characters
/// (`A-Z a-z 0-9 - _ . ~`) stay as-is, everything else becomes `%XX` — space
/// included (`%20`, form-safe).
pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-decodes into bytes: every `%XX` becomes its byte, `+` becomes a space
/// (query-string convention, llcom's `UrlDecode` parity), other characters pass
/// through as their UTF-8 bytes. A `%` without two hex digits is an error.
pub fn url_decode_bytes(text: &str) -> Result<Vec<u8>, UrlDecodeError> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '+' => out.push(b' '),
            '%' => {
                let hi = chars.next().ok_or(UrlDecodeError(' '))?;
                let lo = chars.next().ok_or(UrlDecodeError(' '))?;
                let hi = hex_digit(hi).ok_or(UrlDecodeError(hi))?;
                let lo = hex_digit(lo).ok_or(UrlDecodeError(lo))?;
                out.push((hi << 4) | lo);
            }
            other => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    Ok(out)
}

/// Percent-decodes into a String ([`url_decode_bytes`] + lossy UTF-8 — the
/// codec page's display form never refuses to show data).
pub fn url_decode(text: &str) -> Result<String, UrlDecodeError> {
    url_decode_bytes(text).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// One hex digit (either case); `None` for anything else.
fn hex_digit(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

// ---- Garbled-text recovery (issue #22; llcom's EncodingFixPage) --------------

/// One garbled-text recovery hypothesis, scored best-first by
/// [`recover_garbled`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCandidate {
    /// The encoding the text was wrongly *displayed* as — re-encoding with it
    /// must yield the original bytes (the hypothesis's roundtrip sanity gate).
    pub misread: &'static str,
    /// The encoding the original bytes really are in.
    pub actual: &'static str,
    /// The recovered text.
    pub result: String,
    /// Plausibility score (higher = the likelier fix; see [`recover_garbled`]).
    pub score: i32,
}

/// The encodings the recovery sweeps — the supported CJK set plus the
/// Windows-1252/ISO-8859-1 way-station (llcom's fixer list, WHATWG names; in
/// encoding_rs the `iso-8859-1` label *is* windows-1252).
const RECOVERY_ENCODINGS: &[(&str, &Encoding)] = &[
    ("UTF-8", encoding_rs::UTF_8),
    ("GBK", encoding_rs::GBK),
    ("GB18030", encoding_rs::GB18030),
    ("Big5", encoding_rs::BIG5),
    ("Shift_JIS", encoding_rs::SHIFT_JIS),
    ("Windows-1252", encoding_rs::WINDOWS_1252),
];

/// Recovers garbled text (mojibake): for every ordered encoding pair it tests
/// the hypothesis "this text is what you get when bytes in encoding `actual`
/// are wrongly displayed as `misread`" — re-encode with the misread charset,
/// decode with the actual one — and scores the survivors, best first.
///
/// Gates and scoring (the ticket's three axes):
/// - *roundtrip sanity* (a hard gate): the text must re-encode with `misread`
///   without errors and decode back unchanged — else the hypothesis is
///   impossible and the candidate is not generated;
/// - *replacement-char absence*: every `U+FFFD` in a result costs 40 (a decode
///   that still substitutes is not a recovery);
/// - *CJK frequency plausibility*: each CJK character (ideographs, kana, hangul,
///   fullwidth/CJK punctuation) adds 8, printable ASCII adds 1, C1 controls and
///   private-use characters cost 40 (classic mis-decode artifacts), other
///   control characters cost 30. Halfwidth katakana (`ｱ`-`ﾟ`, U+FF61-U+FF9F)
///   add only 3 — a run of them is what a Shift_JIS mis-decode produces, not
///   recovered Japanese;
/// - two tie-breakers: +30 when `actual` is UTF-8 and the recovered bytes are
///   themselves valid UTF-8 (UTF-8 is self-validating — that the bytes parse as
///   UTF-8 at all is near-certain evidence they really are UTF-8), +2 when the
///   result re-encodes back to the exact bytes (nothing was lost).
///
/// Limits (honest ones): a *lossy* mis-display (e.g. GBK bytes shown as UTF-8
/// with `U+FFFD` substitution) has already destroyed bytes no heuristic can
/// conjure back — those candidates simply score out; and among equally clean
/// all-CJK results the ranking stays a heuristic — every candidate remains in
/// the list for the user to judge. Input that is pure ASCII is identical in
/// every encoding here, so nothing changes and no candidate is produced.
pub fn recover_garbled(text: &str) -> Vec<RecoveryCandidate> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for &(misread, misread_encoding) in RECOVERY_ENCODINGS {
        // The hypothesis needs the text to survive the misread charset both
        // ways (roundtrip sanity): errors or a changed roundtrip mean the text
        // cannot be that charset's view of any bytes.
        let (bytes, _, encode_errors) = misread_encoding.encode(text);
        if encode_errors {
            continue;
        }
        if misread_encoding.decode(&bytes).0 != text {
            continue;
        }
        let bytes = bytes.into_owned();
        for &(actual, actual_encoding) in RECOVERY_ENCODINGS {
            if actual == misread {
                continue;
            }
            let (decoded, _, decode_errors) = actual_encoding.decode(&bytes);
            if decoded == text {
                continue; // no change, not a fix
            }
            let mut score = text_plausibility(&decoded);
            if decode_errors {
                score -= 40; // replacement-char absence
            }
            if actual == "UTF-8" && std::str::from_utf8(&bytes).is_ok() {
                score += 30; // UTF-8 is self-validating
            }
            let (reencoded, _, reencode_errors) = actual_encoding.encode(&decoded);
            if !reencode_errors && reencoded == bytes {
                score += 2; // exact re-encode roundtrip: nothing was lost
            }
            candidates.push(RecoveryCandidate {
                misread,
                actual,
                result: decoded.into_owned(),
                score,
            });
        }
    }
    // Stable sort: ties keep enumeration order (UTF-8 actuals before GB18030
    // look-alikes, the earlier/more common charsets first).
    candidates.sort_by(|a, b| b.score.cmp(&a.score));
    candidates
}

/// The CJK ranges that make a recovered string plausible for these charsets.
fn is_cjk_plausible(ch: char) -> bool {
    let c = ch as u32;
    (0x3000..=0x303f).contains(&c)     // CJK punctuation
        || (0x3040..=0x30ff).contains(&c)  // kana
        || (0x3400..=0x4dbf).contains(&c)  // CJK extension A
        || (0x4e00..=0x9fff).contains(&c)  // CJK unified ideographs
        || (0xac00..=0xd7af).contains(&c)  // hangul syllables
        || (0xf900..=0xfaff).contains(&c)  // CJK compatibility ideographs
        || (0xff00..=0xffef).contains(&c)  // fullwidth forms
}

/// The halfwidth katakana forms (`ｱ`-`ﾟ`, U+FF61-U+FF9F): CJK-adjacent, but a
/// run of them is what a Shift_JIS mis-decode *produces* — weighted low in
/// [`text_plausibility`].
fn is_halfwidth_kana(ch: char) -> bool {
    let c = ch as u32;
    (0xff61..=0xff9f).contains(&c)
}

/// Plausibility of a recovery result: how much it looks like *intended text*
/// rather than mis-decoded bytes (the scoring weights are documented on
/// [`recover_garbled`]).
fn text_plausibility(text: &str) -> i32 {
    let mut score = 0;
    for ch in text.chars() {
        let c = ch as u32;
        if ch == '\u{fffd}'
            || (0x80..=0x9f).contains(&c)
            || (0xe000..=0xf8ff).contains(&c)
        {
            score -= 40; // replacement char, C1 controls, private use
        } else if is_halfwidth_kana(ch) {
            score += 3;
        } else if is_cjk_plausible(ch) {
            score += 8;
        } else if ch.is_ascii_graphic() || ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
            score += 1;
        } else if ch.is_control() {
            score -= 30;
        }
        // Everything else (accented Latin, other scripts): neutral.
    }
    score
}

/// The `RCOM_CODECDEMO` autodemo (issue #22): the conversion and recovery paths
/// proving themselves on known mojibake, headlessly — no port, no network. The
/// evidence lands in the trace log (`AUTODEMO CODEC OK` lines) and as `##` rows
/// in the session log view, the same as the net/MQTT demos.
///
/// The five proofs: UTF-8 → GBK bytes are byte-faithful; a GBK→UTF-8 transcode
/// round-trips; base64 and percent-encoding round-trip; the classic mojibake
/// "你好" (UTF-8 read as GBK → `浣犲ソ`) recovers; and the reverse direction's
/// recoverable form ("专业" GBK bytes read as UTF-8 → `רҵ`) recovers too.
pub mod demo {
    use crate::core::bus::{Event, EventBus, LogEvent, LogLevel};
    use crate::core::codec::{
        base64_decode, base64_encode, encode_text, hex_encode, parse_hex, recover_garbled,
        transcode, TextEncoding,
    };

    /// The demo mode requested by `RCOM_CODECDEMO`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CodecDemoMode {
        Off,
        Basic,
    }

    impl CodecDemoMode {
        /// Reads `RCOM_CODECDEMO`: `1` = the conversion + recovery proofs, else off.
        pub fn from_env() -> Self {
            match std::env::var("RCOM_CODECDEMO").as_deref() {
                Ok("1") => Self::Basic,
                _ => Self::Off,
            }
        }
    }

    /// One `##` row into the session log (the demos' evidence convention).
    fn note(bus: &EventBus, text: String, level: LogLevel) {
        bus.publish(Event::Log(LogEvent {
            level,
            timestamp: chrono::Local::now(),
            message: text,
        }));
    }

    /// Runs the proofs synchronously (pure functions — microseconds; called
    /// beside the driver when `RCOM_CODECDEMO=1`).
    pub fn run(bus: &EventBus) {
        if CodecDemoMode::from_env() == CodecDemoMode::Off {
            return;
        }
        tracing::info!("RCOM_CODECDEMO=1: driving the codec demo (conversion + recovery)");
        let mut passed = 0u32;
        let mut failed = 0u32;

        // 1. Convert UTF-8 text → GBK bytes, byte-faithful.
        let outcome = encode_text("你好", TextEncoding::Gbk);
        step(
            bus,
            &mut passed,
            &mut failed,
            outcome.bytes == vec![0xc4, 0xe3, 0xba, 0xc3] && outcome.unencodable.is_empty(),
            &format!(
                "convert UTF-8 → GBK: '你好' = {} (byte-faithful)",
                hex_encode(&outcome.bytes)
            ),
        );

        // 2. Transcode the GBK bytes back to UTF-8: same bytes UTF-8 would make.
        let round = transcode(&[0xc4, 0xe3, 0xba, 0xc3], TextEncoding::Gbk, TextEncoding::Utf8);
        step(
            bus,
            &mut passed,
            &mut failed,
            round.bytes == b"\xe4\xbd\xa0\xe5\xa5\xbd" && round.text == "你好",
            &format!(
                "transcode GBK → UTF-8: c4 e3 ba c3 → {} ('{}')",
                hex_encode(&round.bytes),
                round.text
            ),
        );

        // 3. Hex + base64 + percent round trips.
        let payload = "\u{0}\u{ff}hello 你好".as_bytes();
        let b64 = base64_encode(payload);
        let url = crate::core::codec::url_encode("a b/你+好~");
        step(
            bus,
            &mut passed,
            &mut failed,
            parse_hex(&hex_encode(payload)).as_deref() == Ok(payload)
                && base64_decode(&b64).as_deref() == Ok(payload)
                && crate::core::codec::url_decode(&url).as_deref() == Ok("a b/你+好~"),
            &format!("codec round trips: hex/base64 ('{b64}')/url ('{url}')"),
        );

        // 4. Recovery, the classic: UTF-8 bytes of 你好 mis-read as GBK → 浣犲ソ.
        let garbled = TextEncoding::Gbk.decode(&TextEncoding::Utf8.encode("你好"));
        let candidates = recover_garbled(&garbled);
        let best = candidates.first();
        step(
            bus,
            &mut passed,
            &mut failed,
            matches!(best, Some(candidate)
                if candidate.misread == "GBK"
                    && candidate.actual == "UTF-8"
                    && candidate.result == "你好"),
            &format!(
                "recover '{garbled}' (UTF-8 read as GBK) → best candidate: {}",
                best.map(candidate_label).unwrap_or_else(|| "<none>".into())
            ),
        );

        // 5. Recovery, the recoverable reverse: GBK bytes that happen to form
        // valid UTF-8 ('专业' → 'רҵ') recover too.
        let garbled2 = TextEncoding::Utf8.decode(&TextEncoding::Gbk.encode("专业"));
        let candidates2 = recover_garbled(&garbled2);
        let best2 = candidates2.first();
        step(
            bus,
            &mut passed,
            &mut failed,
            matches!(best2, Some(candidate)
                if candidate.misread == "UTF-8"
                    && candidate.actual == "GBK"
                    && candidate.result == "专业"),
            &format!(
                "recover '{garbled2}' (GBK bytes read as UTF-8) → best candidate: {}",
                best2.map(candidate_label).unwrap_or_else(|| "<none>".into())
            ),
        );

        if failed == 0 {
            tracing::info!("AUTODEMO CODEC OK: all {passed} codec proofs passed");
            note(
                bus,
                format!("## AUTODEMO CODEC OK: all {passed} proofs (convert/transcode/round trips/recovery)"),
                LogLevel::Info,
            );
        } else {
            tracing::error!("AUTODEMO CODEC FAILED: {failed} of {} proofs failed", passed + failed);
            note(
                bus,
                format!("!! AUTODEMO CODEC FAILED: {failed} proof(s) failed"),
                LogLevel::Error,
            );
        }
    }

    /// One proof: on success an `AUTODEMO CODEC OK` trace line; on failure the
    /// detail and a FAILED line (both also as a `##`/`!!` row).
    fn step(bus: &EventBus, passed: &mut u32, failed: &mut u32, ok: bool, detail: &str) {
        if ok {
            *passed += 1;
            tracing::info!("AUTODEMO CODEC OK: {detail}");
        } else {
            *failed += 1;
            let message = format!("AUTODEMO CODEC FAILED: {detail}");
            tracing::error!("{message}");
            note(bus, format!("!! {message}"), LogLevel::Error);
        }
    }

    /// The one-line form of a candidate (shared shape with the page's rows).
    fn candidate_label(candidate: &super::RecoveryCandidate) -> String {
        format!(
            "{} → {} ({})",
            candidate.misread, candidate.actual, candidate.result
        )
    }
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

    // ---- The conversion toolbox (issue #22) ---------------------------------

    /// Byte-faithful conversion: text → each charset's exact bytes; hex_encode
    /// inverts parse_hex.
    #[test]
    fn conversions_are_byte_faithful() {
        let outcome = encode_text("你好", TextEncoding::Gbk);
        assert_eq!(outcome.bytes, vec![0xc4, 0xe3, 0xba, 0xc3]);
        assert!(outcome.unencodable.is_empty());

        let kana = encode_text("ｺﾝ", TextEncoding::ShiftJis);
        assert_eq!(TextEncoding::ShiftJis.decode(&kana.bytes), "ｺﾝ");

        // hex_encode is the uniform inverse of parse_hex.
        assert_eq!(hex_encode(&[0x0f, 0x0a, 0xff]), "0f 0a ff");
        assert_eq!(parse_hex(&hex_encode(b"rcom")), Ok(b"rcom".to_vec()));
    }

    /// Un-encodable characters are reported, and encoding_rs's numeric-entity
    /// fallback means the bytes still exist (never an error).
    #[test]
    fn unencodable_characters_are_reported() {
        // WHATWG GBK (cp936) covers euro and kana; the Hebrew alef it does not.
        let outcome = encode_text("א", TextEncoding::Gbk);
        assert_eq!(outcome.unencodable, vec!['א']);
        assert!(!outcome.bytes.is_empty(), "entity fallback still emits bytes");

        let fine = encode_text("€", TextEncoding::Gbk);
        assert!(fine.unencodable.is_empty(), "cp936 has the euro sign");

        // ASCII converts everywhere with nothing un-encodable.
        for encoding in TextEncoding::all() {
            assert!(encode_text("ok", *encoding).unencodable.is_empty(), "{}", encoding.id());
        }
    }

    /// The transcode chain: GBK bytes → text → UTF-8 bytes, with the decode and
    /// encode sides reporting honestly.
    #[test]
    fn transcode_moves_bytes_between_charsets() {
        let round = transcode(&[0xc4, 0xe3, 0xba, 0xc3], TextEncoding::Gbk, TextEncoding::Utf8);
        assert_eq!(round.text, "你好");
        assert_eq!(round.bytes, b"\xe4\xbd\xa0\xe5\xa5\xbd");
        assert!(!round.decode_replacements);
        assert!(round.unencodable.is_empty());

        // Bytes that are not really `from`-encoded flag replacements, and the
        // transcode still produces the lossy chain's bytes. (Not [0xff, 0xfe]:
        // that is the UTF-16LE BOM, which encoding_rs's BOM sniffing honours.)
        let lossy = transcode(&[0xff], TextEncoding::Utf8, TextEncoding::Gbk);
        assert!(lossy.decode_replacements);
        assert!(lossy.text.contains('\u{fffd}'));
    }

    /// Base64 round trips; decode tolerates missing padding and whitespace.
    #[test]
    fn base64_round_trips_and_tolerates_padding() {
        let cases: [&[u8]; 4] =
            [b"", b"f", b"fo", "foo bar \u{ff}\u{00} 你好".as_bytes()];
        for case in cases {
            assert_eq!(base64_decode(&base64_encode(case)).as_deref(), Ok(case), "{case:?}");
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        // Padded and unpadded decode alike; whitespace is ignored.
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Ok(b"hello".as_slice()));
        assert_eq!(base64_decode("aGVsbG8").as_deref(), Ok(b"hello".as_slice()));
        assert_eq!(base64_decode("aGVs bG8=").as_deref(), Ok(b"hello".as_slice()));
        assert!(base64_decode("not base64!!").is_err());
    }

    /// Percent-encoding round trips; the unreserved set stays readable, space
    /// round-trips through %20, '+' decodes as a space, bad escapes error.
    #[test]
    fn url_round_trips_and_rejects_bad_escapes() {
        let text = "a b/你+好~.-_x";
        assert_eq!(url_encode(text), "a%20b%2F%E4%BD%A0%2B%E5%A5%BD~.-_x");
        assert_eq!(url_decode(&url_encode(text)), Ok(text.to_string()));
        assert_eq!(url_decode("100%25"), Ok("100%".to_string()));
        assert_eq!(url_decode("a+b"), Ok("a b".to_string()));
        // Non-UTF-8 percent bytes degrade lossily instead of erroring.
        assert!(url_decode("%ff").unwrap().contains('\u{fffd}'));
        // A truncated or non-hex escape is an error, not silence.
        assert_eq!(url_decode("%2"), Err(UrlDecodeError(' ')));
        assert_eq!(url_decode("%zz"), Err(UrlDecodeError('z')));
        assert_eq!(url_decode("end%"), Err(UrlDecodeError(' ')));
    }

    // ---- Garbled-text recovery (issue #22) ----------------------------------

    /// The AC's classic: UTF-8 bytes of 你好 mis-read as GBK show 浣犲ソ — the
    /// recovery's best candidate puts the original back.
    #[test]
    fn recovery_fixes_utf8_read_as_gbk() {
        let garbled = TextEncoding::Gbk.decode(&TextEncoding::Utf8.encode("你好"));
        assert_eq!(garbled, "浣犲ソ");
        let candidates = recover_garbled(&garbled);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].misread, "GBK");
        assert_eq!(candidates[0].actual, "UTF-8");
        assert_eq!(candidates[0].result, "你好");
    }

    /// The recoverable reverse: GBK bytes that happen to form valid UTF-8
    /// ("专业" → "רҵ") recover with the actual charset named GBK.
    #[test]
    fn recovery_fixes_gbk_read_as_utf8_when_lossless() {
        let bytes = TextEncoding::Gbk.encode("专业");
        let garbled = TextEncoding::Utf8.decode(&bytes);
        assert_eq!(garbled, "רҵ");
        let candidates = recover_garbled(&garbled);
        assert_eq!(candidates[0].misread, "UTF-8");
        assert_eq!(candidates[0].actual, "GBK");
        assert_eq!(candidates[0].result, "专业");
    }

    /// The GBK↔Big5 mixup: GBK bytes displayed as Big5 (斕疑) recover — the
    /// exact fix candidate (Big5 → GBK → 你好) is in the list. Ranking it above
    /// the equally clean all-CJK look-alikes would need language frequency
    /// tables, so the page lists everything sorted and the user judges — the
    /// two UTF-8 headline cases above are where the scoring is decisive.
    #[test]
    fn recovery_fixes_gbk_read_as_big5() {
        let bytes = TextEncoding::Gbk.encode("你好");
        let garbled = TextEncoding::Big5.decode(&bytes);
        assert_eq!(garbled, "斕疑");
        let candidates = recover_garbled(&garbled);
        assert!(candidates.iter().any(|candidate| {
            candidate.misread == "Big5" && candidate.actual == "GBK" && candidate.result == "你好"
        }));
        // And the list is still best-first.
        let scores: Vec<i32> = candidates.iter().map(|candidate| candidate.score).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);
    }

    /// A lossy mis-display cannot be conjured back: the U+FFFD candidates sink,
    /// and pure ASCII produces no candidates at all (nothing to fix).
    #[test]
    fn recovery_scores_honestly_and_skips_noops() {
        // GBK bytes shown as UTF-8 lossy: the replacement chars destroyed bytes.
        let garbled = TextEncoding::Utf8.decode(&TextEncoding::Gbk.encode("你好"));
        let candidates = recover_garbled(&garbled);
        assert!(candidates.iter().all(|candidate| !candidate.result.contains('\u{fffd}')
            || candidate.score < 0));

        // ASCII is identical in every swept encoding: no candidates.
        assert!(recover_garbled("plain serial log 123").is_empty());
        // Empty input: no candidates, no panic.
        assert!(recover_garbled("").is_empty());

        // The list is sorted best-first.
        let scores: Vec<i32> = recover_garbled("浣犲ソ").iter().map(|c| c.score).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);
    }
}
