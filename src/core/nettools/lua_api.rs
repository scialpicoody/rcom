//! The `rcom.net` script API: the sandboxed one-shot network table installed
//! next to the other builtins (docs/design.md, "Lua layer": "`rcom.net`
//! (http/tcp one-shots)").
//!
//! Shipped surface (v1, this ticket):
//!
//! ```lua
//! local resp, err = rcom.net.tcp_send(host, port, data[, timeout_ms])
//! local resp, err = rcom.net.udp_send(host, port, data[, timeout_ms])
//! local resp, err = rcom.net.tls_send(host, port, data[, timeout_ms])
//! ```
//!
//! - `host` is a name or IP literal (`::1` included); `data` is a string
//!   (binary-safe); `timeout_ms` defaults to 2000.
//! - On success `resp` is the raw response bytes as a string (read until the
//!   remote closes or the timeout passes with no new data); on failure `resp`
//!   is `nil` and `err` carries the reason.
//! - No other network surface is exposed to scripts: the sandbox keeps `io`
//!   and arbitrary `require` closed, and these three calls are the only way a
//!   script touches the network (bounded by the timeout, no listeners).
//!
//! The calls block the calling thread for at most the timeout — the runtime's
//! `api*` contract; run scripts that use them on the worker (the script panel
//! and the demo VM already do).

use mlua::{FromLua, Lua, Value};

use super::oneshot;

/// The default timeout mirroring [`oneshot::DEFAULT_TIMEOUT_MS`].
const DEFAULT_TIMEOUT_MS: f64 = oneshot::DEFAULT_TIMEOUT_MS as f64;

/// Installs the `rcom` global with its `net` table (idempotent per VM).
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let net = lua.create_table()?;

    net.set(
        "tcp_send",
        lua.create_function(|lua, args: OneShotArgs| run_one_shot(lua, args, Kind::Tcp))?,
    )?;
    net.set(
        "udp_send",
        lua.create_function(|lua, args: OneShotArgs| run_one_shot(lua, args, Kind::Udp))?,
    )?;
    net.set(
        "tls_send",
        lua.create_function(|lua, args: OneShotArgs| run_one_shot(lua, args, Kind::Tls))?,
    )?;

    let rcom = lua.create_table()?;
    rcom.set("net", net)?;
    lua.globals().set("rcom", rcom)?;
    Ok(())
}

/// Which one-shot to run.
enum Kind {
    Tcp,
    Udp,
    Tls,
}

/// The uniform argument shape of the one-shots: `(host, port, data[, timeout])`.
struct OneShotArgs {
    host: String,
    port: u16,
    data: Vec<u8>,
    timeout_ms: f64,
}

impl mlua::FromLuaMulti for OneShotArgs {
    fn from_lua_multi(values: mlua::MultiValue, lua: &Lua) -> mlua::Result<Self> {
        let mut iter = values.into_iter();
        let host = match iter.next() {
            Some(Value::String(s)) => s.to_str()?.to_string(),
            Some(other) => {
                return Err(mlua::Error::FromLuaConversionError {
                    from: other.type_name(),
                    to: "string".into(),
                    message: Some("host must be a string".into()),
                })
            }
            None => return Err(mlua::Error::runtime("rcom.net.*_send(host, port, data[, timeout])")),
        };
        let port = match iter.next() {
            Some(value) => u16::from_lua(value, lua)?,
            None => return Err(mlua::Error::runtime("port missing")),
        };
        let data = match iter.next() {
            Some(Value::String(s)) => s.as_bytes().as_ref().to_vec(),
            Some(other) => {
                return Err(mlua::Error::FromLuaConversionError {
                    from: other.type_name(),
                    to: "string".into(),
                    message: Some("data must be a string".into()),
                })
            }
            None => return Err(mlua::Error::runtime("data missing")),
        };
        let timeout_ms = match iter.next() {
            Some(Value::Nil) | None => DEFAULT_TIMEOUT_MS,
            Some(value) => f64::from_lua(value, lua)?,
        };
        let timeout_ms = if timeout_ms.is_finite() && timeout_ms > 0.0 {
            timeout_ms
        } else {
            DEFAULT_TIMEOUT_MS
        };
        Ok(Self { host, port, data, timeout_ms })
    }
}

/// Runs one one-shot and shapes the Lua result: `(response)` on success,
/// `(nil, reason)` on failure.
fn run_one_shot(lua: &Lua, args: OneShotArgs, kind: Kind) -> mlua::Result<mlua::MultiValue> {
    let timeout = std::time::Duration::from_millis(args.timeout_ms.max(1.0) as u64);
    let outcome = match kind {
        Kind::Tcp => oneshot::tcp_send(&args.host, args.port, &args.data, timeout),
        Kind::Udp => oneshot::udp_send(&args.host, args.port, &args.data, timeout),
        Kind::Tls => oneshot::tls_send(&args.host, args.port, &args.data, timeout),
    };
    let mut values = mlua::MultiValue::new();
    match outcome {
        Ok(bytes) => {
            values.push_front(Value::String(lua.create_string(&bytes)?));
        }
        Err(error) => {
            values.push_front(Value::String(lua.create_string(error.as_bytes())?));
            values.push_front(Value::Nil);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::lua::LuaRuntime;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Collects the script prints of a runtime (the OutBox pattern of the lua
    /// runtime tests, in miniature).
    struct Prints {
        rx: mpsc::Receiver<String>,
        bus: EventBus,
    }

    impl Prints {
        fn new() -> Self {
            let bus = EventBus::new();
            let (tx, rx) = mpsc::channel();
            let mut events = bus.subscribe();
            std::thread::spawn(move || {
                while let Ok(event) = events.blocking_recv() {
                    if let crate::core::bus::Event::ScriptOut(out) = event {
                        if let crate::core::bus::ScriptOutKind::Print { line } = out.kind {
                            if tx.send(line).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            Self { rx, bus }
        }

        /// Next print containing `needle` (skips sys.lua's own "core loaded!"
        /// banner and other unrelated lines).
        fn next_matching(&self, within: Duration, needle: &str) -> String {
            loop {
                match self.rx.recv_timeout(within) {
                    Ok(line) if line.contains(needle) => return line,
                    Ok(_) => continue,
                    Err(_) => panic!("no print containing {needle:?} within {within:?}"),
                }
            }
        }
    }

    /// THE script-API proof: a script calls `rcom.net.tcp_send` against a real
    /// echo server and receives the reply as a Lua string.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn script_tcp_send_receives_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 128];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = stream.write_all(&buf[..n]);
            }
        });

        let outbox = Prints::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            &format!(
                r#"
                local resp, err = rcom.net.tcp_send("127.0.0.1", {port}, "ping", 2000)
                if resp == nil then error("tcp_send failed: " .. (err or "?")) end
                print("RESP:" .. resp)
                "#
            ),
        )
        .expect("one-shot script must run");

        let line = outbox.next_matching(Duration::from_secs(5), "RESP:");
        assert_eq!(line, "RESP:ping");
        runtime.stop();
    }

    /// A failing one-shot yields `nil, err` (never a Lua error, never a panic).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn script_tcp_send_reports_errors_as_nil_err() {
        let outbox = Prints::new();
        let runtime = LuaRuntime::start(
            outbox.bus.clone(),
            None,
            None,
            r#"
            local resp, err = rcom.net.tcp_send("127.0.0.1", 1, "ping", 300)
            assert(resp == nil, "expected nil response")
            assert(err ~= nil and #err > 0, "expected an error reason")
            print("ERR-OK")
            "#,
        )
        .expect("error script must run");

        let line = outbox.next_matching(Duration::from_secs(10), "ERR-OK");
        assert_eq!(line, "ERR-OK");
        runtime.stop();
    }

    /// The table is exactly the documented surface (the sandboxed API stays
    /// narrow — no listeners, no arbitrary sockets from scripts).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn surface_is_exactly_the_three_one_shots() {
        let lua = Lua::new();
        install(&lua).unwrap();
        let script = r#"
            local names = {}
            for name, _ in pairs(rcom.net) do table.insert(names, name) end
            table.sort(names)
            return table.concat(names, ",")
        "#;
        let surface: String = lua.load(script).eval().unwrap();
        assert_eq!(surface, "tcp_send,tls_send,udp_send");
    }
}
