//! uartctl：uarttool 的命令行客户端，通过 HTTP 接口读取串口日志、向设备发送命令。

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ureq::http::Response;
use ureq::{Agent, Body, RequestBuilder};

const AFTER_HELP: &str = "\
日志行格式：
  2026-10-23 14:57:01.123 [RX] 设备输出      [TX] 发给设备的数据      [SYS] 服务事件

退出码：0 成功；1 出错；2 参数错误；3 wait/send --expect 超时（期间收到的日志仍会输出）

示例：
  uartctl list                                      查看有哪些串口
  uartctl tail esp32 -n 100                         最近 100 行日志
  uartctl log esp32 --since 1520                    1520 之后的新日志（末行给出下次的 since）
  uartctl grep esp32 \"(?i)error|panic\"              搜索最近的错误
  uartctl send esp32 reboot --expect \"boot done\" --timeout 20
                                                    重启设备并等待启动完成
  uartctl send esp32 '\\x03' --escape --eol none     发送 Ctrl+C
  uartctl wait esp32 \"connected\" --timeout 60       等待某条日志出现
  uartctl follow esp32                              持续输出（Ctrl+C 结束）";

#[derive(Parser)]
#[command(
    version,
    about = "读取 uarttool 共享的串口日志、向设备发送命令",
    after_help = AFTER_HELP
)]
struct Cli {
    /// uarttool 服务地址
    #[arg(long, global = true, env = "UARTCTL_HOST", default_value = "127.0.0.1:7000")]
    host: String,
    /// 访问 token（服务配置了 token 时需要）
    #[arg(long, global = true, env = "UARTCTL_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// 输出 JSON 而不是文本
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出共享的串口及状态
    List,
    /// 列出本机所有串口设备（COM 号、USB 序列号，用于填写配置）
    Devices,
    /// 显示最近 N 行日志
    Tail {
        name: String,
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
    /// 增量读取：输出 seq 大于 --since 的行，最后一行是 `-- next_seq=<n>`
    Log {
        name: String,
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// 在最近的日志中按正则搜索
    Grep {
        name: String,
        pattern: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// 等待一条匹配的新日志出现，输出等待期间的所有日志
    Wait {
        name: String,
        pattern: String,
        /// 超时秒数
        #[arg(long, default_value_t = 30.0, value_parser = parse_timeout)]
        timeout: f64,
        /// 让 TX/SYS 行也参与匹配（默认只匹配设备输出 RX）
        #[arg(long)]
        any_dir: bool,
    },
    /// 持续输出新日志
    Follow { name: String },
    /// 向设备发送数据（默认在末尾追加 \r\n）
    Send {
        name: String,
        text: String,
        /// 发送后等待匹配这个正则的日志行，并输出期间的日志
        #[arg(long)]
        expect: Option<String>,
        /// 等待 --expect 的超时秒数
        #[arg(long, default_value_t = 5.0, value_parser = parse_timeout)]
        timeout: f64,
        /// 让 TX/SYS 行也参与 --expect 匹配（默认只匹配设备输出 RX）
        #[arg(long)]
        any_dir: bool,
        /// 末尾追加的换行：crlf、cr、lf、none
        #[arg(long, default_value = "crlf")]
        eol: String,
        /// 解析转义：\r \n \t \\ \xNN
        #[arg(long)]
        escape: bool,
    },
    /// 显示服务端的接口说明
    Doc,
}

/// HTTP 请求本身的超时要比等待时间长一些，留出网络往返。
const REQUEST_SLACK: f64 = 10.0;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("uartctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let client = Client::new(&cli);
    match cli.command {
        Command::List => client.print(client.get("/ports", None)),
        Command::Devices => client.print(client.get("/devices", None)),
        Command::Doc => client.print(client.get("/", None)),
        Command::Tail { name, lines } => client.print(
            client
                .get(&port_path(&name, "tail"), None)
                .query("lines", lines.to_string()),
        ),
        Command::Log { name, since, limit } => client.print(
            client
                .get(&port_path(&name, "log"), None)
                .query("since", since.to_string())
                .query("limit", limit.to_string()),
        ),
        Command::Grep { name, pattern, limit } => client.print(
            client
                .get(&port_path(&name, "grep"), None)
                .query("pattern", pattern)
                .query("limit", limit.to_string()),
        ),
        Command::Wait { name, pattern, timeout, any_dir } => client.print(
            client
                .get(&port_path(&name, "wait"), Some(timeout + REQUEST_SLACK))
                .query("pattern", pattern)
                .query("timeout", timeout.to_string())
                .query("dir", dir_param(any_dir)),
        ),
        Command::Follow { name } => client.follow(&name),
        Command::Send { name, text, expect, timeout, eol, escape, any_dir } => {
            let data = if escape { unescape(&text)? } else { text.into_bytes() };
            let mut request = client
                .post(&port_path(&name, "send"), Some(timeout + REQUEST_SLACK))
                .query("eol", eol);
            if let Some(expect) = expect {
                request = request
                    .query("expect", expect)
                    .query("timeout", timeout.to_string())
                    .query("dir", dir_param(any_dir));
            }
            client.finish(request.send(&data[..]))
        }
    }
}

fn dir_param(any_dir: bool) -> &'static str {
    if any_dir {
        "any"
    } else {
        "rx"
    }
}

/// 超时秒数：有限的非负数，上限交给服务端。
fn parse_timeout(s: &str) -> std::result::Result<f64, String> {
    match s.parse::<f64>() {
        Ok(t) if t.is_finite() && t >= 0.0 => Ok(t),
        _ => Err(format!("`{s}` 不是有效的秒数")),
    }
}

fn port_path(name: &str, action: &str) -> String {
    format!("/ports/{name}/{action}")
}

struct Client {
    agent: Agent,
    base: String,
    token: Option<String>,
    json: bool,
}

impl Client {
    fn new(cli: &Cli) -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5)))
            .build()
            .into();
        let base = if cli.host.starts_with("http://") || cli.host.starts_with("https://") {
            cli.host.trim_end_matches('/').to_string()
        } else {
            format!("http://{}", cli.host.trim_end_matches('/'))
        };
        Self { agent, base, token: cli.token.clone(), json: cli.json }
    }

    fn get(&self, path: &str, timeout: Option<f64>) -> RequestBuilder<ureq::typestate::WithoutBody> {
        let mut request = self.agent.get(format!("{}{path}", self.base));
        request = request.config().timeout_global(timeout.map(Duration::from_secs_f64)).build();
        self.decorate(request)
    }

    fn post(&self, path: &str, timeout: Option<f64>) -> RequestBuilder<ureq::typestate::WithBody> {
        let mut request = self.agent.post(format!("{}{path}", self.base));
        request = request.config().timeout_global(timeout.map(Duration::from_secs_f64)).build();
        self.decorate(request)
    }

    fn decorate<B>(&self, mut request: RequestBuilder<B>) -> RequestBuilder<B> {
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if self.json {
            request = request.query("format", "json");
        }
        request
    }

    fn print(&self, request: RequestBuilder<ureq::typestate::WithoutBody>) -> Result<ExitCode> {
        self.finish(request.call())
    }

    /// 输出响应体，并把 HTTP 状态映射成退出码。
    fn finish(&self, response: Result<Response<Body>, ureq::Error>) -> Result<ExitCode> {
        let mut response = response.with_context(|| format!("无法连接 uarttool 服务 {}", self.base))?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string()?;
        match status {
            200..=299 => {
                print!("{body}");
                Ok(ExitCode::SUCCESS)
            }
            408 => {
                print!("{body}");
                Ok(ExitCode::from(3))
            }
            _ => bail!("HTTP {status}: {}", body.trim_end().trim_start_matches("错误: ")),
        }
    }

    fn follow(&self, name: &str) -> Result<ExitCode> {
        let response = self
            .get(&port_path(name, "stream"), None)
            .call()
            .with_context(|| format!("无法连接 uarttool 服务 {}", self.base))?;
        if !response.status().is_success() {
            return self.finish(Ok(response));
        }
        let mut reader = response.into_body().into_reader();
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Ok(ExitCode::SUCCESS);
            }
            stdout.write_all(&buf[..n])?;
            stdout.flush()?;
        }
    }
}

/// 解析 `\r \n \t \\ \xNN` 转义。
fn unescape(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = u8::from_str_radix(&hex, 16)
                    .with_context(|| format!("无效的转义 \\x{hex}"))?;
                out.push(byte);
            }
            Some(other) => bail!("不支持的转义 \\{other}"),
            None => bail!("末尾多了一个 \\"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_sequences() {
        assert_eq!(unescape(r"a\r\n\t\\\x03中").unwrap(), b"a\r\n\t\\\x03\xE4\xB8\xAD");
        assert!(unescape(r"\xZZ").is_err());
        assert!(unescape(r"\q").is_err());
        assert!(unescape("\\").is_err());
    }
}
