# uarttool

把本机的串口通过局域网共享出去：WindTerm / XShell 等终端、局域网内其它设备以及 AI Agent
可以同时使用同一个串口，主要用于设备调试时抓取日志。

- `crates/uart-core`：服务核心（串口、日志、Terminal 服务、HTTP 接口）
- `crates/uarttool`：托盘常驻程序（也可以 `--headless` 运行）
- `crates/uartctl`：命令行工具，供脚本和 AI 使用

设计见 [docs/DESIGN.md](docs/DESIGN.md)。
