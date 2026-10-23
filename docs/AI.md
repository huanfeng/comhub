# 让 AI 使用 uarttool

uarttool 不依赖 MCP。AI 只要能执行 shell 命令，就可以通过 `uartctl` 或 `curl` 使用它。

## 方式一：Claude Code Skill（推荐）

把 `skills/uart-debug/` 复制到项目的 `.claude/skills/`，或者复制到 `~/.claude/skills/`（对所有项目生效）。
Claude Code 会在需要查看串口日志时自动加载。

## 方式二：写进 AGENTS.md / CLAUDE.md

不支持 Skill 的 Agent，可以把下面这段放进项目的 `AGENTS.md` 或 `CLAUDE.md`：

```markdown
## 串口设备调试

设备串口由 uarttool 服务独占并共享，不要直接打开 COM 口，统一用 `uartctl`：
- `uartctl list` 查看设备及状态
- `uartctl tail <name> -n 200` 最近日志；`uartctl log <name> --since <seq>` 增量读取
- `uartctl grep <name> "(?i)error"` 搜索
- `uartctl send <name> "<cmd>" --expect "<正则>" --timeout 10` 发命令并等待结果（超时退出码 2）
- `uartctl wait <name> "<正则>" --timeout 60` 等待某条日志
完整说明见 `uartctl --help` 和 `uartctl doc`。
```

## 方式三：只有 HTTP

局域网中的其它机器上没有 `uartctl` 时，先让 AI 读一下接口说明：

```sh
curl http://<host>:7000/
```

返回内容包括全部接口、示例和当前串口列表。
