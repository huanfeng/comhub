# uarttool

把本机的串口通过局域网共享出去：WindTerm / XShell 等终端、局域网内其它设备以及 AI Agent
可以**同时**使用同一个串口，主要用于设备调试时抓取日志。

- `crates/uart-core`：服务核心（串口、日志、Terminal 服务、HTTP 接口）
- `crates/uarttool`：托盘常驻程序（也可以 `--headless` 运行）
- `crates/uartctl`：命令行工具，供脚本和 AI 使用

设计见 [docs/DESIGN.md](docs/DESIGN.md)，AI 接入见 [docs/AI.md](docs/AI.md)。

## 构建

```sh
cargo build --release
# 产物：target/release/uarttool.exe、target/release/uartctl.exe
```

## 使用

1. 运行 `uarttool.exe`，托盘区出现图标，主窗口自动打开。
2. 点「添加串口」：选择设备，填写别名和 TCP 端口，保存后立即生效。
   有 USB 序列号的设备按序列号绑定，换 USB 口、COM 号变化后也能自动认回来。
3. 在 WindTerm / XShell 中新建 **Telnet** 会话，主机填 `127.0.0.1`（局域网其它机器填本机 IP），
   端口填该串口的 TCP 端口。终端的编码请和设备输出一致（配置中的 `encoding`）。
4. AI 或脚本使用 `uartctl`：

   ```sh
   uartctl list
   uartctl tail esp32 -n 100
   uartctl send esp32 reboot --expect "boot done" --timeout 20
   ```

关闭主窗口后程序继续在托盘运行；退出请用托盘菜单的「退出」。托盘菜单里可以开启开机自启。

### 文件位置

| 内容 | 位置 |
|---|---|
| 配置文件 | `%APPDATA%\uarttool\config.toml`（`--config` 可指定） |
| 设备日志 | 配置文件旁的 `logs\<别名>\<别名>-YYYY-MM-DD.log` |
| 程序日志 | 配置文件旁的 `uarttool.log` |

### 命令行参数

```
uarttool [--config <path>] [--autostart] [--headless] [--list]
  --autostart   只显示托盘，不打开主窗口（开机自启使用）
  --headless    不启动界面，只运行服务，日志输出到控制台
  --list        列出本机串口及 USB 序列号
```

## 安全

服务默认监听 `0.0.0.0`，局域网内任何设备都可以读写串口。需要限制时：

- `[server] bind = "127.0.0.1"`：只允许本机访问；
- `[server] token = "..."`：HTTP 接口需要 token（`uartctl --token` 或环境变量 `UARTCTL_TOKEN`）。
  终端端口（Telnet/Raw）不支持认证。

HTTP 接口会拒绝带 `Origin` 头的请求和非 IP / 本机名的 `Host`，防止本机浏览器中的网页借道访问。
