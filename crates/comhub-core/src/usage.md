# ComHub HTTP 接口

ComHub 把本机串口共享到网络上。通过这个接口可以读取设备日志、向设备发送命令。
所有接口默认返回纯文本；加 `format=json` 返回 JSON。下面示例中的 `{base}` 是本服务地址。

## 日志行格式

```
2026-10-23 14:57:01.123 [RX] 设备输出的一行
2026-10-23 14:57:03.004 [TX] 发给设备的一行
2026-10-23 14:57:03.500 [SYS] 服务事件（连接、断开等）
```

每个串口的每一行都有一个递增的序号 seq，用于增量读取。

## 接口

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/ports` | 串口列表和状态 |
| GET | `/devices` | 本机所有串口设备（用于配置） |
| GET | `/ports/{name}/tail?lines=200` | 最近 N 行 |
| GET | `/ports/{name}/log?since=<seq>&limit=1000` | seq 大于 since 的行；最后一行是 `-- next_seq=<n>`，下次带上 `since=<n>` |
| GET | `/ports/{name}/grep?pattern=<regex>&limit=200` | 在最近的日志中搜索（正则） |
| GET | `/ports/{name}/wait?pattern=<regex>&timeout=30` | 等待一条新的匹配行出现，返回等待期间的所有行 |
| GET | `/ports/{name}/stream` | 持续输出新日志（`curl -N`） |
| POST | `/ports/{name}/send?expect=<regex>&timeout=5&eol=crlf` | 请求体发给设备；带 expect 时等待匹配行并返回期间的日志 |

- `eol`：在请求体末尾追加的换行，可选 `crlf`（默认）、`cr`、`lf`、`none`。
- `wait` 和带 `expect` 的 `send` 默认只用设备输出（RX 行）匹配，加 `dir=any` 让 TX/SYS 行也参与。
  超时返回 HTTP 408，串口被停止返回 503，响应体仍是期间收到的日志。
- `log` 的游标之后若有行已被挤出内存缓冲，开头会有一行 `-- gap: ...` 提示，完整内容在日志文件中。
- 正则语法是 Rust regex，例如 `(?i)error|panic`。参数需要 URL 编码（curl 可用 `-G --data-urlencode`）。

## 示例

```sh
curl {base}/ports
curl "{base}/ports/esp32/tail?lines=100"
curl "{base}/ports/esp32/log?since=1520"
curl -G {base}/ports/esp32/grep --data-urlencode "pattern=(?i)error"
curl -X POST --data "reboot" "{base}/ports/esp32/send?expect=boot%20done&timeout=20"
curl -N {base}/ports/esp32/stream
```

如果本机有 `comhub` 命令行工具，用法更简单，见 `comhub --help`。
