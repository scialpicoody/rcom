//! The Lua engine (docs/design.md, "Lua layer"): a sandboxed Lua 5.3 VM per serial
//! session plus a standalone VM, running llcom's Luat `sys.*` task framework
//! unmodified.
//!
//! Layout: [`scripts`] embeds the llcom Lua sources (licenses: `assets/LICENSES.md`),
//! [`sandbox`] prunes the standard library, [`json`] is the serde-backed `JSON`
//! builtin, [`runtime`] is the VM + host API, [`hook`] is the TX pre-send seam.
//!
//! Spike outcome for ticket #18 (measured in the tests):
//! - `sys.wait(ms)` = `coroutine.yield()` in the task coroutine + a host-side
//!   `apiStartTimer` that arms a one-shot tokio timer; on expiry it fires the
//!   `tiggerCB` global which `coroutine.resume`s the task (llcom's exact
//!   mechanism). No busy waits; measured jitter is tokio timer precision
//!   (sub-2 ms typical, see `sys_wait_resume_via_tokio_timer_is_on_time`).
//! - mlua 0.10 has no usable `set_interrupt` (Luau-only) and `Lua::set_hook`
//!   covers only the main thread — `hook_proc` strips hooks inherited by
//!   coroutines (`(*extra).hook_thread != state → lua_sethook(None)`). The stop
//!   mechanism is therefore an instruction-count hook armed on whichever thread
//!   is about to run: Rust entry points arm `current_thread()`, and `head.lua`
//!   re-arms through `__armStopHook(co)` on every `coroutine.resume`. Setting
//!   the stop flag unwinds any running Lua within ~250k instructions (see
//!   `instruction_hook_stops_hard_loops`).

pub mod hook;
pub mod json;
pub mod ports_api;
pub mod runtime;
pub mod sandbox;
pub mod scripts;

pub use hook::{TxHook, run_send_convert};
pub use runtime::{DetachedSink, LuaRuntime, QuickSendLookup, UartSink};

#[cfg(test)]
mod spike {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use mlua::{Lua, VmState};

    /// THE mechanism under test, llcom-shaped: Lua task coroutine suspends in
    /// `coroutine.yield` (what `sys.wait` does), a tokio timer resumes it through
    /// the `tigger` global (what `sys.tigger` does) — measured end to end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sys_wait_resume_via_tokio_timer_is_on_time() {
        let lua = Arc::new(Lua::new());
        let jitter: Arc<Mutex<Vec<i128>>> = Arc::new(Mutex::new(Vec::new()));

        // apiStartTimer, llcom LuaRunEnv.StartTimer equivalent: one-shot tokio timer.
        // The deadline is armed-time + ms, exactly how llcom (and sys.wait) re-arm.
        let api_jitter = Arc::clone(&jitter);
        let api_start_timer = {
            let api_lua = Arc::clone(&lua);
            let closure_lua = Arc::clone(&api_lua);
            api_lua
                .create_function(move |_lua, (id, ms): (i64, u64)| {
                    let lua = Arc::clone(&closure_lua);
                    let jitter = Arc::clone(&api_jitter);
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
                    tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        jitter.lock().unwrap().push(
                            (Instant::now().saturating_duration_since(deadline.into_std()))
                                .as_micros()
                                as i128,
                        );
                        if let Ok(fire) = lua.globals().get::<mlua::Function>("tigger") {
                            if let Err(e) = fire.call::<()>(id) {
                                panic!("resume failed: {e}");
                            }
                        }
                    });
                    Ok(1)
                })
                .unwrap()
        };
        lua.globals().set("apiStartTimer", api_start_timer).unwrap();

        // The sys.wait core: suspend in yield, re-arm from inside the task — the
        // exact data path of llcom's sys.lua (yield → apiStartTimer → tigger → resume).
        let t0 = Instant::now();
        lua.load(
            r#"
            waits = 0
            task = coroutine.create(function()
                while waits < 10 do
                    coroutine.yield()      -- sys.wait(ms) suspends here
                    waits = waits + 1
                    apiStartTimer(waits, 100) -- schedule the next wake, 100ms from now
                end
            end)
            tigger = function(id)
                assert(coroutine.resume(task))
            end
            assert(coroutine.resume(task)) -- start the task (sys.taskInit)
            apiStartTimer(1, 100)
        "#,
        )
        .exec()
        .unwrap();

        let done = async {
            while lua.globals().get::<i64>("waits").unwrap() < 10 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .expect("task did not finish in time");

        let jitter = jitter.lock().unwrap().clone();
        assert_eq!(jitter.len(), 10);
        let max_us = *jitter.iter().max().unwrap();
        // tokio's timer wheel is coarse (1ms buckets) but never busy-waits; allow
        // 20ms of scheduler slack and require the waits to have actually waited.
        assert!(max_us < 20_000, "wait jitter {max_us}us too large: {jitter:?}");
        let total = t0.elapsed();
        assert!(
            total >= Duration::from_millis(950),
            "10 waits of 100ms finished too fast: {total:?}"
        );
        println!(
            "SPIKE #18: 10x sys.wait(100ms) via tokio timer: max jitter {max_us}us, total {total:?}"
        );
    }

    /// The hard-loop breaker: an instruction-count hook whose error unwinds any
    /// running Lua. The stop flag is set from another thread while the loop
    /// burns a worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn instruction_hook_stops_hard_loops() {
        let lua = Arc::new(Lua::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_stop = Arc::clone(&stop);
        lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(1_000),
            move |_lua, _debug| {
                if hook_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    Err(mlua::Error::RuntimeError("script stopped".into()))
                } else {
                    Ok(VmState::Continue)
                }
            },
        );
        // A `while true do end` in the main thread, as a panel script would run.
        let run_lua = Arc::clone(&lua);
        let loop_task = tokio::task::spawn_blocking(move || {
            run_lua.load("while true do end").exec()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t_stop = Instant::now();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("hook must break the loop")
            .unwrap()
            .expect_err("stop flag must abort the loop");
        assert!(err.to_string().contains("script stopped"), "{err}");
        let stopped_within = t_stop.elapsed();
        assert!(
            stopped_within < Duration::from_secs(1),
            "stop took {stopped_within:?}"
        );
        println!("SPIKE #18: hard loop stopped {stopped_within:?} after flag set");
    }

    /// Async functions work on this feature set (the #19 TX-hook seam rides on
    /// `call_async` from the session write path).
    #[tokio::test]
    async fn async_functions_are_drivable_from_rust() {
        let lua = Lua::new();
        let f = lua
            .create_async_function(|_lua, input: String| async move {
                Ok(input.to_uppercase())
            })
            .unwrap();
        let out: String = f.call_async("abc").await.unwrap();
        assert_eq!(out, "ABC");
    }
}
