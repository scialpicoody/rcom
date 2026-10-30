//! The Lua sources embedded into the binary. `sys`/`strings`/`log` are llcom's
//! unmodified core scripts (licenses in `assets/LICENSES.md`); `head` is the
//! rcom port. The llcom sample is the README's demo script — the AC runs it
//! verbatim, so it is embedded verbatim too.

/// llcom's Luat task framework, unmodified (driven by the host's `apiStartTimer` /
/// `apiStopTimer` and `tiggerCB`).
pub const SYS_LUA: &str = include_str!("assets/sys.lua");
/// String extensions: `:toHex()`, `:fromHex()`, `:split()`, ... (unmodified).
pub const STRINGS_LUA: &str = include_str!("assets/strings.lua");
/// Leveled logging on top of `print` (unmodified).
pub const LOG_LUA: &str = include_str!("assets/log.lua");
/// rcom's port of llcom's `head.lua`: the pre-user-script setup.
pub const HEAD_LUA: &str = include_str!("assets/head.lua");

/// llcom README's standalone-script sample ("独立的Lua脚本自动处理串口收发"),
/// byte-for-byte. AC: runs unchanged.
pub const LLCOM_README_SAMPLE: &str = r#"
--注册串口接收函数
uartReceive = function (data)
    log.info("uartReceive",data)
    sys.publish("UART",data)--发布消息
end

--新建任务，等待接收到消息再继续运行
sys.taskInit(function()
    while true do
        local _,udata = sys.waitUntil("UART")--等待消息
        log.info("task waitUntil",udata)
        local sendResult = apiSendUartData("ok!")--发送串口消息
        log.info("uart send",sendResult)
    end
end)

--新建任务，每休眠1000ms继续一次
sys.taskInit(function()
    while true do
        sys.wait(1000)--等待1000ms
        log.info("task wait",os.time())
    end
end)

--1000ms循环定时器
sys.timerLoopStart(log.info,1000,"timer test")
"#;

/// The send-conversion snippets from llcom's README (the pre-send hook seam #19
/// builds on): `uartData` in, transformed bytes returned.
pub const SEND_CONVERT_HEX: &str = "return uartData:fromHex()";
pub const SEND_CONVERT_DEFAULT: &str = "return uartData";
