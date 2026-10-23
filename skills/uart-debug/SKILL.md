---
name: uart-debug
description: 通过 ComHub 读取串口设备日志、向设备发送命令。当需要查看设备串口输出、排查设备启动或运行问题、重启设备并观察日志、等待设备某条日志出现时使用。
---

# 用 ComHub 调试串口设备

本机（或局域网内）运行着 ComHub 服务，它独占物理串口并把日志共享出来。
**不要直接打开 COM 口**——串口已被 ComHub 占用；所有读写都通过 `comhub` 完成。

## 先确认有哪些设备

```sh
comhub list
```

`STATE` 为 `connected(COMx)` 才能收发；`waiting` 表示设备未接入。
服务不在本机时加 `--host <ip>:7000`（或设置环境变量 `COMHUB_HOST`）。

## 常用操作

| 目的 | 命令 |
|---|---|
| 看最近的日志 | `comhub tail <name> -n 200` |
| 只读新增的日志 | `comhub log <name> --since <seq>`，末行 `-- next_seq=<n>` 就是下次的 since |
| 搜索错误 | `comhub grep <name> "(?i)error\|fail\|panic"` |
| 发命令并等结果 | `comhub send <name> "<cmd>" --expect "<正则>" --timeout 10` |
| 等某条日志出现 | `comhub wait <name> "<正则>" --timeout 60` |
| 发送 Ctrl+C | `comhub send <name> '\x03' --escape --eol none` |

日志行格式：`2026-10-23 14:57:01.123 [RX] ...`，`[RX]` 是设备输出，`[TX]` 是写给设备的内容
（包括其他人在终端里敲的命令），`[SYS]` 是服务事件（设备连接/断开）。

## 工作方式

- 优先用 `send --expect`：它在服务端等待结果，一次调用就能拿到命令输出，不需要反复轮询。
- 持续观察时用 `log --since` 增量读取，不要反复 `tail` 大量行，节省上下文。
- 退出码 3 表示等待超时，此时输出的仍是等待期间收到的日志，通常足以判断问题。
- `--expect` 和 `wait` 默认只匹配设备输出（`[RX]`），不会被你发出的命令本身误命中；
  需要匹配 TX/SYS 行时加 `--any-dir`。
- 串口可能同时被开发者在终端里使用，日志中出现不是你发的 `[TX]` 属正常现象；
  发送重启、擦除等有副作用的命令前，先确认这是用户想要的。
- 完整的日志文件按天保存在服务端的日志目录（`<log_dir>/<name>/<name>-YYYY-MM-DD.log`），
  本机可以直接 grep；HTTP 接口说明见 `comhub doc`。
