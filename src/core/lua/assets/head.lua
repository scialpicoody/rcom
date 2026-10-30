-- head.lua — rcom port of llcom's DefaultFiles/core_script/head.lua (Apache-2.0,
-- https://github.com/chenxuuu/llcom). It runs in every VM before the user script
-- and sets up everything llcom scripts expect. Differences from the original,
-- kept to what rcom's sandbox/runtime actually provides:
--   * no package.path/filesystem require setup: modules are preloaded by the host
--   * no debug.sethook anti-deadloop: the Rust runtime arms instruction-count
--     stop hooks instead (mlua strips Lua-inherited hooks on coroutines)
--   * apiUtf8ToHex encodes UTF-8 bytes (no GBK conversion; encoding_rs lands later)
-- Everything else — print override, channel registry, tiggerCB, uart compat — is
-- ported as-is so llcom scripts run unmodified.

--加强随机数随机性
math.randomseed(tostring(os.time()):reverse():sub(1, 6))

--加载字符串工具包
require("strings")

--提前退出：发送转换VM只需要strings，宿主会注入uartData并取返回值
if runType == "send" then return end

--重写print函数
function print(...)
    arg = { ... }
    local logAll = {}
    for i=1,select('#', ...) do
        table.insert(logAll, tostring(arg[i]))
    end
    apiPrintLog(table.concat(logAll, "\t"))
end

log = require("log")
sys = require("sys")

--重写获取快捷发送区数据接口
local oldapiQuickSendList = apiQuickSendList
apiQuickSendList = function (id)
    local s = oldapiQuickSendList(id)
    if s:sub(1,1) == "S" then
        return s:sub(2)
    elseif s:sub(1,1) == "H" then
        return s:sub(2):fromHex()
    else
        return
    end
end

--设置回调
local channelCb = {}
function apiSetCb(channel, cb)
    if not channelCb[channel] then
        channelCb[channel] = {}
    end
    --把每个注册的回调都保存下来
    table.insert(channelCb[channel],cb)
end
--取消某个回调
function apiUnsetCb(channel, cb)
    if not channelCb[channel] then
        return true
    end
    for i=1,#channelCb[channel] do
        if channelCb[channel][i] == cb then
            table.remove(channelCb[channel],i)
            if #channelCb[channel] == 0 then
                channelCb[channel] = nil
            end
            return true
        end
    end
end

--协程外部触发
tiggerCB = function (id,type,data)
    local result, info = pcall(function ()
        if id >= 0 then--定时器消息
            sys.tigger(id)
        else--其他消息（通用通道）
            if channelCb[type] then
                --把每个注册的函数都运行一遍
                for i=1,#channelCb[type] do
                    channelCb[type][i](data)
                end
            end
        end
    end)
    if not result then
        log.error("task","run failed\r\n"..apiAscii2Utf8(tostring(info)))
    end
end

--执行命令
apiSetCb("cmd",function(data)
    load(data)()
    log.info("console","run success")
end)

--兼容老的串口接口
apiSendUartData = function(data)
    return apiSend("uart",data)
end
apiSetCb("uart",function(data)
    if uartReceive then uartReceive(data) end
end)

--硬循环保护：mlua会把主线程hook从协程上剥掉（hook_proc只认hook_thread），
--所以每次resume前由宿主给目标线程重新布防stop hook。
local rawresume = coroutine.resume
coroutine.resume = function(co, ...)
    __armStopHook(co)
    return rawresume(co, ...)
end
