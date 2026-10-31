//! The protocol-decoder host (issue #24): one Lua VM running a decoder script
//! that turns byte streams into named plot channels.
//!
//! A decoder is an ordinary Lua script (managed like every other script:
//! persisted under `<config>/rcom/scripts/`, loadable in the script editor)
//! that defines one global function:
//!
//! ```lua
//! function decode(bytes, timestamp_ms) ... end
//! ```
//!
//! The host feeds it every RX chunk (via the session bus tap on the core
//! driver) with the chunk's epoch-millisecond timestamp; the script parses
//! what it can — buffering partial frames in its own globals across calls —
//! and appends points through the `rcom.ui` API (`add_point` & friends,
//! installed by [`super::lua_api`]).
//!
//! Failure semantics: a corrupt frame is the decoder's business (the bundled
//! one skips and keeps going); a *script* error inside `decode` fails the
//! decoder — the driver stops it and reports
//! `PlotEventKind::DecoderFailed` once, instead of erroring per chunk.

use crate::core::bus::EventBus;
use crate::core::lua::LuaRuntime;

/// A running protocol decoder: the VM handle plus its display name.
#[derive(Clone)]
pub struct DecoderHost {
    name: String,
    vm: LuaRuntime,
}

impl std::fmt::Debug for DecoderHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderHost").field("name", &self.name).finish_non_exhaustive()
    }
}

impl DecoderHost {
    /// Boots a decoder VM with `code` and checks the `decode(bytes, ts)`
    /// contract. `create` is bounded setup; the script chunk itself is the
    /// unbounded part, so callers run this on a blocking task (the driver's
    /// pattern for the script panel).
    pub fn start(name: impl Into<String>, bus: EventBus, code: &str) -> mlua::Result<Self> {
        let vm = LuaRuntime::create(bus, None, None)?;
        vm.run_script(code)?;
        if !vm.has_function("decode") {
            vm.stop();
            return Err(mlua::Error::runtime(
                "decoder script must define a global `decode(bytes, timestamp_ms)` function",
            ));
        }
        Ok(Self {
            name: name.into(),
            vm,
        })
    }

    /// Feeds one RX chunk into `decode`. A script error here fails the decoder
    /// (the caller stops it and reports once).
    pub fn feed(&self, bytes: &[u8], timestamp_ms: f64) -> mlua::Result<()> {
        self.vm.call_bytes_function("decode", bytes, timestamp_ms)
    }

    /// Stops the VM (a stopped decoder ignores further feeds).
    pub fn stop(&self) {
        self.vm.stop();
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::plot::frames::{build_corrupt_frame, build_frame};
    use crate::core::plot::store::global_store;

    fn now_ms() -> f64 {
        chrono::Utc::now().timestamp_millis() as f64
    }

    fn channel_count(name: &str) -> usize {
        global_store()
            .channel_names()
            .iter()
            .find(|(channel, _)| channel == name)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    }

    /// The bundled decoder parses well-formed frames into named channels —
    /// THE AC: "a bundled example decoder script turns a simple framed
    /// protocol into plotted channels".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundled_decoder_plots_well_formed_frames() {
        let host = DecoderHost::start(
            "test: framed",
            EventBus::new(),
            crate::core::lua::scripts::FRAMED_DECODER_SAMPLE,
        )
        .expect("bundled decoder must boot");
        global_store().clear_channel("ch0");

        let t0 = now_ms();
        let chunk = [
            build_frame(0, 100).as_slice(),
            build_frame(1, -100).as_slice(),
            build_frame(0, 150).as_slice(),
        ]
        .concat();
        host.feed(&chunk, t0).expect("decode must succeed");

        assert_eq!(channel_count("ch0"), 2);
        assert_eq!(channel_count("ch1"), 1);
        let snapshot = global_store().sample(1_000_000.0, 100);
        let ch0 = snapshot
            .series
            .iter()
            .find(|series| series.name == "ch0")
            .expect("ch0 plotted");
        assert_eq!(ch0.points, vec![(t0, 100.0), (t0, 150.0)]);
        global_store().clear_channel("ch0");
        global_store().clear_channel("ch1");
        host.stop();
    }

    /// A corrupt frame is skipped without killing the decoder — the stream
    /// resyncs and later frames still plot (the AC's "rejects a corrupt one").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundled_decoder_skips_corrupt_frames() {
        let host = DecoderHost::start(
            "test: framed",
            EventBus::new(),
            crate::core::lua::scripts::FRAMED_DECODER_SAMPLE,
        )
        .expect("bundled decoder must boot");
        global_store().clear_channel("ch2");

        let t0 = now_ms();
        let chunk = [
            build_corrupt_frame(2, 500).as_slice(),
            build_frame(2, 700).as_slice(),
        ]
        .concat();
        host.feed(&chunk, t0).expect("corruption must not error the decoder");

        assert_eq!(channel_count("ch2"), 1, "only the well-formed frame plots");
        let snapshot = global_store().sample(1_000_000.0, 10);
        let ch2 = snapshot
            .series
            .iter()
            .find(|series| series.name == "ch2")
            .expect("ch2 plotted");
        assert_eq!(ch2.points, vec![(t0, 700.0)]);
        global_store().clear_channel("ch2");
        host.stop();
    }

    /// Partial frames survive across decode calls (stateful chunk boundaries).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundled_decoder_buffers_partial_frames() {
        let host = DecoderHost::start(
            "test: framed",
            EventBus::new(),
            crate::core::lua::scripts::FRAMED_DECODER_SAMPLE,
        )
        .expect("bundled decoder must boot");
        global_store().clear_channel("ch3");

        let t0 = now_ms();
        let frame = build_frame(3, -1234);
        let (head, tail) = frame.split_at(4);
        host.feed(head, t0).expect("head feed");
        assert_eq!(channel_count("ch3"), 0, "partial frame must not plot yet");
        host.feed(tail, t0).expect("tail feed");
        assert_eq!(channel_count("ch3"), 1);
        let snapshot = global_store().sample(1_000_000.0, 10);
        assert_eq!(snapshot.series[0].points[0].1, -1234.0);
        global_store().clear_channel("ch3");
        host.stop();
    }

    /// A decoder without the `decode` global fails at start (the contract
    /// check), and a decoder erroring inside decode surfaces the error to the
    /// caller (which stops it once).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decoder_contract_is_enforced() {
        let error = DecoderHost::start("broken", EventBus::new(), "print('no decode here')")
            .expect_err("missing decode must fail the start");
        assert!(error.to_string().contains("decode"), "{error}");

        let host = DecoderHost::start("explodes", EventBus::new(), "function decode(b, t) error('boom', 0) end")
            .expect("loading a broken decode body is fine");
        let error = host.feed(b"x", 1.0).expect_err("decode must error");
        assert!(error.to_string().contains("boom"), "{error}");
        host.stop();
    }
}
