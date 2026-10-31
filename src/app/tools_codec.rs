//! The codec toolbox page (issue #22): the encoding-conversion / hex / base64 /
//! percent codecs plus llcom's garbled-text recovery, as the bottom tools pane's
//! fifth tab.
//!
//! Like the sibling tool pages the XAML carries the skeleton (named controls, no
//! visible strings); this module resolves them, sets every label from the i18n
//! dictionaries and wires the events. The page is entirely synchronous — every
//! operation is a pure `core::codec` function call (microseconds on tool-sized
//! inputs), so no pump message, hub or spawn_blocking is involved: the page owns
//! no core state at all.
//!
//! Copy semantics: the output box is read-only but selectable (`Ctrl+C` via the
//! TextBox's copy gesture copies the selection).
//!
//! Byte model (the AC's "byte-faithful"): the input's bytes are
//! `parse_hex(input)` when the HEX toggle is on, else `encode(input, From)`;
//! text-producing decodes re-decode with the To charset, byte-producing ops
//! (`Convert`, `Text → HEX/Base64`) surface the exact result bytes as hex.

use std::cell::RefCell;
use std::rc::Rc;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::media::FontFamily;
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, CheckBox, ComboBox, ListBox, TabItem, TextBox, TextBlock};

use rust_i18n::t;

use rcom::core::codec::{
    base64_decode, base64_encode, encode_text, hex_encode, parse_hex, recover_garbled, transcode,
    url_decode_bytes, url_encode, RecoveryCandidate, TextEncoding,
};

/// The input/output boxes' monospaced families, first available wins (the pin's
/// TextBox is not XAML-font-settable — the script editor sets its font the same
/// way in the controller).
const CODEC_EDITOR_FONT: &str = "Menlo, Consolas, DejaVu Sans Mono, Courier New";

/// The operations the op selector offers, selector order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodecOp {
    /// Charset conversion A→B (the two selectors; byte-faithful hex result).
    Convert,
    /// Text (or hex input) → spaced hex bytes.
    HexEncode,
    /// Hex input → text decoded with the To charset.
    HexDecode,
    /// Text (or hex input) → standard base64.
    Base64Encode,
    /// Base64 input → text decoded with the To charset.
    Base64Decode,
    /// Text → percent-encoded URL text.
    UrlEncode,
    /// Percent-encoded input → text decoded with the To charset.
    UrlDecode,
    /// The garbled-text recovery: scored candidates into the list.
    Recover,
}

impl CodecOp {
    /// Selector order (indexes are the selector's).
    fn all() -> [CodecOp; 8] {
        [
            CodecOp::Convert,
            CodecOp::HexEncode,
            CodecOp::HexDecode,
            CodecOp::Base64Encode,
            CodecOp::Base64Decode,
            CodecOp::UrlEncode,
            CodecOp::UrlDecode,
            CodecOp::Recover,
        ]
    }

    /// The i18n key of the selector label.
    fn label_key(self) -> &'static str {
        match self {
            CodecOp::Convert => "codec_op_convert",
            CodecOp::HexEncode => "codec_op_hex_enc",
            CodecOp::HexDecode => "codec_op_hex_dec",
            CodecOp::Base64Encode => "codec_op_b64_enc",
            CodecOp::Base64Decode => "codec_op_b64_dec",
            CodecOp::UrlEncode => "codec_op_url_enc",
            CodecOp::UrlDecode => "codec_op_url_dec",
            CodecOp::Recover => "codec_op_recover",
        }
    }

    /// The opposite direction, for the swap button (`None` for the one-way ops).
    fn opposite(self) -> Option<CodecOp> {
        match self {
            CodecOp::HexEncode => Some(CodecOp::HexDecode),
            CodecOp::HexDecode => Some(CodecOp::HexEncode),
            CodecOp::Base64Encode => Some(CodecOp::Base64Decode),
            CodecOp::Base64Decode => Some(CodecOp::Base64Encode),
            CodecOp::UrlEncode => Some(CodecOp::UrlDecode),
            CodecOp::UrlDecode => Some(CodecOp::UrlEncode),
            CodecOp::Convert | CodecOp::Recover => None,
        }
    }
}

/// The op the selector offers at an index (mirrors [`CodecOp::all`]).
fn op_at(index: i32) -> CodecOp {
    CodecOp::all().get(index.max(0) as usize).copied().unwrap_or(CodecOp::Convert)
}

/// The charset a selector offers at an index (mirrors [`TextEncoding::all`]).
fn charset_at(index: i32) -> TextEncoding {
    TextEncoding::all().get(index.max(0) as usize).copied().unwrap_or(TextEncoding::Utf8)
}

/// The charset selector's index for `encoding` (the swap button's input).
fn index_of(encoding: TextEncoding) -> i32 {
    TextEncoding::all()
        .iter()
        .position(|candidate| *candidate == encoding)
        .unwrap_or(0) as i32
}

/// One list row for a recovery candidate: score, hypothesis, result.
fn candidate_row(candidate: &RecoveryCandidate) -> String {
    let result = candidate.result.replace('\n', "\\n").replace('\r', "\\r");
    format!(
        "{score:>4}  {misread} → {actual}   {result}",
        score = candidate.score,
        misread = candidate.misread,
        actual = candidate.actual,
    )
}

/// One page instance: the resolved controls plus the recovery state.
struct CodecPage {
    tab: Ref<TabItem>,
    op: Ref<ComboBox>,
    from: Ref<ComboBox>,
    swap: Ref<Button>,
    to: Ref<ComboBox>,
    input_hex: Ref<CheckBox>,
    run: Ref<Button>,
    input_caption: Ref<TextBlock>,
    status: Ref<TextBlock>,
    input: Ref<TextBox>,
    output_caption: Ref<TextBlock>,
    output: Ref<TextBox>,
    candidates: Ref<ListBox>,
    /// The recovery candidates behind the list rows (index-aligned with them).
    recovery: RefCell<Vec<RecoveryCandidate>>,
}

impl CodecPage {
    /// Applies the locale strings and fills the selectors (the app re-creates
    /// the window per locale change — the rest of the app works the same way).
    fn apply_ui_strings(&self) {
        self.tab.set_header(Some(Rc::new(t!("codec_tab").to_string())));
        self.input_hex.set_content(Some(Rc::new(t!("codec_hex").to_string())));
        self.run.set_content(Some(Rc::new(t!("codec_run").to_string())));
        self.swap.set_content(Some(Rc::new(t!("codec_swap").to_string())));
        self.input_caption.set_text(Some(&t!("codec_input")));
        self.output_caption.set_text(Some(&t!("codec_output")));
        self.status.set_text(Some(&t!("codec_status_idle")));
        self.input.set_placeholder_text(Some(&t!("codec_input_hint")));

        let ops = self.op.items();
        ops.clear();
        for codec_op in CodecOp::all() {
            let value: BoxedValue = Rc::new(t!(codec_op.label_key()).to_string());
            ops.add(Some(value));
        }
        self.op.set_selected_index(0);

        for selector in [&self.from, &self.to] {
            let items = selector.items();
            items.clear();
            for encoding in TextEncoding::all() {
                // Encoding names are proper nouns — not localized (the same
                // rule the toolbar's encoding selector follows).
                let value: BoxedValue = Rc::new(encoding.display_name().to_string());
                items.add(Some(value));
            }
        }
        self.from.set_selected_index(0);
        self.to.set_selected_index(0);
    }

    fn set_status(&self, text: String) {
        self.status.set_text(Some(&text));
    }

    fn set_output(&self, text: String) {
        self.output.set_text(Some(&text));
    }

    /// The input's bytes: `parse_hex` under the HEX toggle, else the text
    /// encoded with `from`. `Err` carries the user-facing reason.
    fn input_bytes(&self, from: TextEncoding, text: &str) -> Result<Vec<u8>, String> {
        if self.input_hex.is_checked() == Some(true) {
            parse_hex(text).map_err(|error| t!("codec_status_bad_hex", error = error).to_string())
        } else {
            Ok(encode_text(text, from).bytes)
        }
    }

    /// The Convert/Run button (and Enter in the input box): runs the selected
    /// operation, status line reports the details.
    fn run(&self) {
        self.recovery.borrow_mut().clear();
        self.candidates.items().clear();
        let Some(text) = self.input.text() else {
            self.set_status(t!("codec_status_empty").to_string());
            return;
        };
        if text.is_empty() {
            self.set_status(t!("codec_status_empty").to_string());
            return;
        }
        let codec_op = op_at(self.op.selected_index());
        let from = charset_at(self.from.selected_index());
        let to = charset_at(self.to.selected_index());
        let result = match codec_op {
            CodecOp::Convert => self.run_convert(from, to, &text),
            CodecOp::HexEncode => self.run_bytes_to_text(from, &text, "hex"),
            CodecOp::HexDecode => self.run_hex_decode(to, &text),
            CodecOp::Base64Encode => self.run_bytes_to_text(from, &text, "base64"),
            CodecOp::Base64Decode => self.run_base64_decode(to, &text),
            CodecOp::UrlEncode => self.run_url_encode(&text),
            CodecOp::UrlDecode => self.run_url_decode(to, &text),
            CodecOp::Recover => self.run_recover(&text),
        };
        match result {
            Ok(status) => self.set_status(status),
            Err(status) => self.set_status(status),
        }
    }

    /// Convert A→B: byte-faithful transcode, the result bytes surface as hex
    /// (the wire form is the point of the conversion); the status line previews
    /// the re-decoded text and reports replacements/un-encodables.
    fn run_convert(&self, from: TextEncoding, to: TextEncoding, text: &str) -> Result<String, String> {
        let bytes_in = self.input_bytes(from, text)?;
        let out = transcode(&bytes_in, from, to);
        self.set_output(hex_encode(&out.bytes));
        let mut notes = vec![t!(
            "codec_status_converted",
            from = from.display_name(),
            to = to.display_name(),
            n = out.bytes.len(),
        )
        .to_string()];
        notes.push(t!("codec_status_text", text = truncate_preview(&out.text)).to_string());
        if out.decode_replacements {
            notes.push(t!("codec_status_replacements").to_string());
        }
        if !out.unencodable.is_empty() {
            notes.push(self.unencodable_note(&out.unencodable, to));
        }
        Ok(notes.join("  ·  "))
    }

    /// Text → HEX / Text → Base64: the input bytes (hex toggle or `from`-encoded)
    /// in the uniform text form.
    fn run_bytes_to_text(&self, from: TextEncoding, text: &str, kind: &str) -> Result<String, String> {
        let bytes = self.input_bytes(from, text)?;
        let output = if kind == "hex" {
            hex_encode(&bytes)
        } else {
            base64_encode(&bytes)
        };
        self.set_output(output);
        Ok(t!("codec_status_bytes", n = bytes.len()).to_string())
    }

    /// HEX → Text: the parsed bytes decoded with the To charset.
    fn run_hex_decode(&self, to: TextEncoding, text: &str) -> Result<String, String> {
        let bytes = parse_hex(text).map_err(|error| t!("codec_status_bad_hex", error = error).to_string())?;
        self.decode_output(to, &bytes)
    }

    /// Base64 → Text: the decoded bytes with the To charset.
    fn run_base64_decode(&self, to: TextEncoding, text: &str) -> Result<String, String> {
        let bytes =
            base64_decode(text).map_err(|error| t!("codec_status_bad_base64", error = error).to_string())?;
        self.decode_output(to, &bytes)
    }

    /// Text → URL: RFC 3986 percent-encoding of the UTF-8 bytes.
    fn run_url_encode(&self, text: &str) -> Result<String, String> {
        let output = url_encode(text);
        self.set_output(output);
        Ok(t!("codec_status_bytes", n = text.len()).to_string())
    }

    /// URL → Text: the percent-decoded bytes with the To charset.
    fn run_url_decode(&self, to: TextEncoding, text: &str) -> Result<String, String> {
        let bytes =
            url_decode_bytes(text).map_err(|error| t!("codec_status_bad_url", error = error).to_string())?;
        self.decode_output(to, &bytes)
    }

    /// The shared decode tail of the → Text ops: decode with `to`, report
    /// replacement characters honestly.
    fn decode_output(&self, to: TextEncoding, bytes: &[u8]) -> Result<String, String> {
        let text = to.decode(bytes);
        self.set_output(text.clone());
        let mut notes = vec![t!("codec_status_bytes", n = bytes.len()).to_string()];
        if to.had_errors(bytes) {
            notes.push(t!("codec_status_replacements").to_string());
        }
        Ok(notes.join("  ·  "))
    }

    /// The garbled-text recovery: fill the candidates list (best first); a
    /// selection applies the candidate to the output box.
    fn run_recover(&self, text: &str) -> Result<String, String> {
        let found = recover_garbled(text);
        let mut recovery = self.recovery.borrow_mut();
        *recovery = found;
        let items = self.candidates.items();
        items.clear();
        for candidate in recovery.iter() {
            let value: BoxedValue = Rc::new(candidate_row(candidate));
            items.add(Some(value));
        }
        if recovery.is_empty() {
            return Ok(t!("codec_status_no_candidates").to_string());
        }
        Ok(t!("codec_status_candidates", n = recovery.len()).to_string())
    }

    /// A candidate row was clicked: apply its result to the output box.
    fn apply_candidate(&self) {
        let index = self.candidates.selected_index();
        if index < 0 {
            return;
        }
        let candidate = self.recovery.borrow().get(index as usize).cloned();
        let Some(candidate) = candidate else { return };
        self.set_output(candidate.result.clone());
        self.set_status(
            t!(
                "codec_status_applied",
                misread = candidate.misread,
                actual = candidate.actual,
            )
            .to_string(),
        );
    }

    /// The ⇄ button: Convert swaps the two charsets; the encode/decode codec
    /// pairs flip their operation; the one-way ops do nothing.
    fn swap(&self) {
        let codec_op = op_at(self.op.selected_index());
        if codec_op == CodecOp::Convert {
            let from = charset_at(self.from.selected_index());
            let to = charset_at(self.to.selected_index());
            self.from.set_selected_index(index_of(to));
            self.to.set_selected_index(index_of(from));
            return;
        }
        if let Some(flipped) = codec_op.opposite() {
            let index = CodecOp::all().iter().position(|candidate| *candidate == flipped);
            if let Some(index) = index {
                self.op.set_selected_index(index as i32);
            }
        }
    }

    /// The "not representable in …" status fragment (unique chars, sample).
    fn unencodable_note(&self, chars: &[char], encoding: TextEncoding) -> String {
        let mut unique: Vec<char> = Vec::new();
        for ch in chars {
            if !unique.contains(ch) {
                unique.push(*ch);
            }
        }
        let shown: String = unique.iter().take(6).collect();
        t!(
            "codec_status_unencodable",
            enc = encoding.display_name(),
            chars = shown,
        )
        .to_string()
    }
}

/// Truncates a preview to one status-line-friendly line.
fn truncate_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = (&mut chars).take(40).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview.replace('\n', "\\n").replace('\r', "\\r")
}

/// Resolves the named controls of the codec tab, sets the strings and wires the
/// events. One insertion point in the controller's `attach`.
pub fn attach(element: &Ref<StyledElement>) {
    let page = Rc::new(CodecPage {
        tab: NameScopeExtensions::get(element, "CodecTab"),
        op: NameScopeExtensions::get(element, "CodecOpSelector"),
        from: NameScopeExtensions::get(element, "CodecFromSelector"),
        swap: NameScopeExtensions::get(element, "CodecSwapButton"),
        to: NameScopeExtensions::get(element, "CodecToSelector"),
        input_hex: NameScopeExtensions::get(element, "CodecHexCheck"),
        run: NameScopeExtensions::get(element, "CodecRunButton"),
        input_caption: NameScopeExtensions::get(element, "CodecInputCaption"),
        status: NameScopeExtensions::get(element, "CodecStatus"),
        input: NameScopeExtensions::get(element, "CodecInputBox"),
        output_caption: NameScopeExtensions::get(element, "CodecOutputCaption"),
        output: NameScopeExtensions::get(element, "CodecOutputBox"),
        candidates: NameScopeExtensions::get(element, "CodecCandidates"),
        recovery: RefCell::new(Vec::new()),
    });
    // Multiline + monospaced input/output, read-only selectable output (the
    // copy semantics): set in code, not XAML-reliable at this pin.
    page.input.set_accepts_return(true);
    page.input.set_font_family(FontFamily::new(CODEC_EDITOR_FONT));
    page.output.set_accepts_return(true);
    page.output.set_font_family(FontFamily::new(CODEC_EDITOR_FONT));
    page.output.set_is_read_only(true);

    page.apply_ui_strings();

    page.run.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.run()
    });
    page.swap.add_handler(Button::click_event(), {
        let page = Rc::clone(&page);
        move |_, _| page.swap()
    });
    page.input.key_down({
        let page = Rc::clone(&page);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                page.run();
            }
        }
    });
    page.candidates.selection_changed({
        let page = Rc::clone(&page);
        move |_, _| page.apply_candidate()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selector index mapping mirrors `CodecOp::all`, unknown indexes fall
    /// back to Convert, and the swap opposites pair up.
    #[test]
    fn op_selector_mapping_and_opposites() {
        for (index, codec_op) in CodecOp::all().iter().enumerate() {
            assert_eq!(op_at(index as i32), *codec_op);
        }
        assert_eq!(op_at(-1), CodecOp::Convert);
        assert_eq!(op_at(99), CodecOp::Convert);
        assert_eq!(op_at(CodecOp::all().len() as i32), CodecOp::Convert);

        assert_eq!(CodecOp::HexEncode.opposite(), Some(CodecOp::HexDecode));
        assert_eq!(CodecOp::Base64Decode.opposite(), Some(CodecOp::Base64Encode));
        assert_eq!(CodecOp::UrlEncode.opposite(), Some(CodecOp::UrlDecode));
        assert_eq!(CodecOp::Convert.opposite(), None);
        assert_eq!(CodecOp::Recover.opposite(), None);
        // Every opposite is reachable in the selector and mirrored back.
        for codec_op in CodecOp::all() {
            if let Some(flipped) = codec_op.opposite() {
                assert_eq!(flipped.opposite(), Some(codec_op));
                assert!(CodecOp::all().contains(&flipped));
            }
        }
    }

    /// The charset mapping mirrors `TextEncoding::all` and round-trips through
    /// `index_of` — the swap button's contract.
    #[test]
    fn charset_selector_mapping_round_trips() {
        for (index, encoding) in TextEncoding::all().iter().enumerate() {
            assert_eq!(charset_at(index as i32), *encoding);
            assert_eq!(index_of(*encoding), index as i32);
        }
        assert_eq!(charset_at(-1), TextEncoding::Utf8);
        assert_eq!(charset_at(42), TextEncoding::Utf8);
    }

    /// Candidate rows carry score, hypothesis and an escaped result.
    #[test]
    fn candidate_rows_format_and_escape() {
        let candidate = RecoveryCandidate {
            misread: "GBK",
            actual: "UTF-8",
            result: "你好".to_string(),
            score: 48,
        };
        let row = candidate_row(&candidate);
        assert!(row.contains("  48"), "{row}");
        assert!(row.contains("GBK → UTF-8"), "{row}");
        assert!(row.contains("你好"), "{row}");

        let newline = RecoveryCandidate {
            misread: "UTF-8",
            actual: "GBK",
            result: "a\r\nb".to_string(),
            score: -7,
        };
        assert!(candidate_row(&newline).contains("a\\r\\nb"));
    }

    /// Previews truncate and escape line breaks (the status text's shape).
    #[test]
    fn status_previews_truncate() {
        assert_eq!(truncate_preview("hello 你好"), "hello 你好");
        let long = "x".repeat(60);
        let preview = truncate_preview(&long);
        assert!(preview.chars().count() <= 41, "{preview}");
        assert!(preview.ends_with('…'));
        assert_eq!(truncate_preview("a\r\nb"), "a\\r\\nb");
    }
}
