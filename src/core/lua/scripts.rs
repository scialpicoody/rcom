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

/// The bundled protocol decoder (issue #24): parses the demo framed protocol
/// (`AA 55 | len u8 | ch u8 | value i16 BE | checksum`, see
/// `core::plot::frames`) into one plotted channel per wire channel
/// (`ch0`, `ch1`, ...). A decoder is an ordinary script defining
/// `decode(bytes, timestamp_ms)`; partial frames buffer in `frame_buf` across
/// calls, corrupt frames are skipped (one `log.warn` per run, not per frame —
/// a noisy wire must not flood the log).
pub const FRAMED_DECODER_SAMPLE: &str = r#"-- rcom bundled decoder: framed protocol -> plotted channels
-- Frame: AA 55 | len u8 | ch u8 | value i16 big-endian | checksum u8
--   len = 3 (ch + 2 value bytes); checksum = (len + ch + hi + lo) & 0xFF
-- Register: define a global decode(bytes, timestamp_ms); feed RX chunks;
--   plot points with rcom.ui.add_point(channel, value).

frame_buf = ""   -- partial frames survive between decode() calls
warned = false   -- one corruption warning per decoder run, not per frame

local function to_signed(word)
  if word >= 32768 then return word - 65536 end
  return word
end

function decode(bytes, timestamp_ms)
  frame_buf = frame_buf .. bytes
  while #frame_buf >= 7 do
    local a, b = frame_buf:byte(1, 2)
    if a ~= 0xAA or b ~= 0x55 then
      frame_buf = frame_buf:sub(2)          -- resync: drop one byte
    else
      local len = frame_buf:byte(3)
      if len ~= 3 then
        if not warned then log.warn("framed decoder", "bad length " .. len .. ", resyncing") warned = true end
        frame_buf = frame_buf:sub(3)
      else
        local ch, hi, lo, sum = frame_buf:byte(4, 7)
        if ((len + ch + hi + lo) % 256) ~= sum then
          if not warned then log.warn("framed decoder", "checksum mismatch, resyncing") warned = true end
          frame_buf = frame_buf:sub(3)      -- corrupt frame: skip and resync
        else
          rcom.ui.add_point("ch" .. ch, to_signed(hi * 256 + lo))
          frame_buf = frame_buf:sub(8)      -- whole frame consumed
        end
      end
    end
  end
end
"#;
