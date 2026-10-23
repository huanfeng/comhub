# uarttool 设计文档

把本机的多个串口通过局域网共享出去：本机的终端工具（WindTerm / XShell）、局域网内其它设备、
以及 AI Agent 可以**同时**使用同一个串口。主要场景是设备调试时抓取日志。

## 1. 目标与非目标

**第一版目标**

- 一个后台服务独占物理串口，把数据分发给多个使用方，所有使用方都可以写入。
- 支持多个串口；用别名标识设备，并绑定 USB 序列号（COM 号变了也能认回设备）。
- 支持热插拔：设备断开后自动重连，终端连接保持不断。
- 每个串口一个固定的 Telnet / Raw TCP 端口，WindTerm、XShell 可以直接连接。
- 面向 AI：纯文本日志文件 + 文本优先的 HTTP 接口 + `uartctl` 命令行工具。
- GUI：基于 windui 的托盘常驻程序。

**第一版不做**：在界面里调波特率、DTR/RTS 控制、RFC2217、虚拟 COM 口、内置日志查看器、
认证以外的安全机制。这些列在 M5。

## 2. 架构

```
uarttool.exe（托盘常驻；也可以 --headless 运行）
├─ UI 线程：windui 事件循环（托盘 + 按需打开的主窗口）
│      ▲ 事件回调（状态变化）→ App::channel
│      ▼ 命令（新增/删除/重载串口）
└─ 服务运行时（tokio，后台线程）—— crate `uart-core`
   ├─ PortManager：管理多个串口
   │   └─ 每个串口一个 PortHub
   │       ├─ 串口线程：查找设备 → 打开 → 阻塞读；出错就关闭并重新查找
   │       ├─ 写入线程：合并所有客户端写入的数据，按顺序写到串口
   │       ├─ 行切分器（RX / TX 各一个）：按换行或空闲超时切行
   │       ├─ 内存环形缓冲：最近 N 行，每行带递增的 seq
   │       └─ 日志文件：按天滚动
   ├─ Terminal 服务：每个串口一个 TCP 端口（telnet / raw）
   └─ HTTP 服务：一个端口（默认 7000）

uartctl.exe（控制台程序，只通过 HTTP 和服务通信）
```

数据有两条路径：

- **原始字节路径**：串口 → 广播 → 各个终端连接。不加时间戳，不做清洗，和直连串口时完全一样。
- **文本路径**：串口 → 解码 → 行切分 → 去掉 ANSI 转义 → 环形缓冲 + 日志文件。供 AI 和 HTTP 使用。

## 3. 日志格式

```
2026-10-23 14:57:01.123 [RX] I (312) wifi: connected, ip=192.168.1.50
2026-10-23 14:57:03.004 [TX] reboot
2026-10-23 14:57:03.500 [SYS] device disconnected
```

- `RX`：设备发出的数据；`TX`：写往设备的数据；`SYS`：服务自身的事件（连接、断开等）。
- 切行规则：RX 遇到 `\n` 切行，遇到 `\r` 丢弃；超过 100ms 没有新数据时，把未结束的内容作为一行写出
  （例如 `login: ` 这种提示符）。TX 遇到 `\r` 或 `\n` 切行，空闲超时为 1s
  （因为终端是逐字符发送的）。单行最长 4096 字节，超过就强制切行。
- 编码：可以按串口配置 `utf-8`（默认）或 `gbk`。无法解码的字节，UTF-8 下表示为 `\xNN`，GBK 下表示为 `�`。
- 文件位置：`<log_dir>/<name>/<name>-YYYY-MM-DD.log`。

## 4. 配置文件

位置：`%APPDATA%\uarttool\config.toml`，可以用 `--config <path>` 指定其它路径。

```toml
[server]
bind = "0.0.0.0"          # HTTP 和 Terminal 服务的监听地址
http_port = 7000
# log_dir = "D:/uartlogs" # 默认是配置文件所在目录下的 logs/
# token = "secret"        # 设置后 HTTP 请求需要带上 token
buffer_lines = 20000      # 每个串口在内存中保留的行数

[[port]]
name = "esp32"            # 别名：只能用字母、数字、- 和 _
serial_number = "A50285BI"   # USB 序列号，优先匹配
# com = "COM5"            # 没有序列号时按 COM 号匹配
baud = 115200
tcp_port = 7001
mode = "telnet"           # telnet | raw
encoding = "utf-8"        # utf-8 | gbk
enabled = true            # false 时保留配置但不打开串口
```

匹配规则：配置了 `serial_number` 就只按序列号匹配；否则按 `com` 匹配。

## 5. Terminal 服务

- 每个串口在 `bind:tcp_port` 上监听，允许多个客户端同时连接。
- `telnet` 模式：连接时发送 `IAC WILL ECHO`、`IAC WILL SGA`、`IAC WILL BINARY` 和 `IAC DO BINARY`，
  让终端进入逐字符模式；发出的数据把 `0xFF` 转义为 `0xFF 0xFF`；收到的数据中去掉 IAC 命令，
  并把 `\r\0` 还原为 `\r`。
- `raw` 模式：原样透传。
- 连接建立时发送一行提示，例如 `[uarttool] esp32 (COM5, 115200) connected`。设备断开或重连时，
  也向所有终端推送一行提示。

## 6. HTTP 接口

默认返回 `text/plain; charset=utf-8`。加上 `?format=json` 返回 JSON。设置了 token 时，需要带
`Authorization: Bearer <token>` 头，或者 `?token=<token>` 参数。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | 使用说明（Markdown），给 AI 读 |
| GET | `/ports` | 所有串口：别名、COM 号、状态、TCP 端口、客户端数量、最新 seq |
| GET | `/ports/{name}/tail?lines=200` | 最近 N 行 |
| GET | `/ports/{name}/log?since=<seq>&limit=1000` | 增量读取，返回 seq 大于 since 的行；响应头带 `X-Next-Seq` |
| GET | `/ports/{name}/grep?pattern=<regex>&limit=200` | 在内存缓冲中搜索 |
| GET | `/ports/{name}/wait?pattern=<regex>&timeout=30` | 等待新出现的匹配行，期间收到的行一并返回 |
| GET | `/ports/{name}/stream` | 持续推送新日志行（`curl -N`） |
| POST | `/ports/{name}/send?expect=<regex>&timeout=5&eol=crlf` | 发送请求体；可选等待匹配 |

- `seq` 是每个串口内从 1 开始递增的行号（服务重启后重新计数）。如果 `since` 比当前最大的 seq 还大（说明服务重启过），就从缓冲区最早的一行开始返回。
- `send` 的 `eol` 参数决定在请求体末尾追加什么：`none`、`cr`、`lf`、`crlf`，默认是 `crlf`。
- `wait` 和 `send?expect=` 默认只用 RX 行匹配（`dir=any` 时所有方向都参与），返回期间所有方向的行。
  超时返回 HTTP 408，串口被停止返回 503，期间收到的行仍然放在响应体里。
- `log` 的游标之后若有行已被挤出内存缓冲，响应开头会有一行 `-- gap: 第 X..Y 行…`（JSON 中为 `gap` 字段）。

## 7. uartctl

```
uartctl [--host <addr>] [--token <t>] [--json] <命令>
  list                                  列出串口
  tail   <name> [-n 200]                最近 N 行
  log    <name> [--since <seq>]         增量读取（输出末尾会打印 next seq）
  grep   <name> <regex>                 搜索
  wait   <name> <regex> [--timeout 30]  等待某行出现
  follow <name>                         持续输出
  send   <name> <text> [--expect <regex>] [--timeout 5] [--eol crlf]
```

`--host` 默认是 `127.0.0.1:7000`，也可以用环境变量 `UARTCTL_HOST` 指定。

## 8. 里程碑

| 阶段 | 内容 |
|---|---|
| M1 | 配置、PortHub、行切分、环形缓冲、日志文件、热插拔、`--headless` |
| M2 | Terminal 服务（telnet / raw） |
| M3 | HTTP 接口、`uartctl`、AI 使用说明 |
| M4 | windui 托盘 GUI、开机自启 |
| M5 | 波特率 / DTR / RTS、RFC2217、mDNS、内置日志查看器 |
