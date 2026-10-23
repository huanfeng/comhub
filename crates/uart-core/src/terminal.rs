//! 终端服务：每个串口一个 TCP 端口，WindTerm / XShell 用 Telnet 或 Raw 方式连接。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::config::{Encoding, TerminalMode};
use crate::port::{PortHub, PortState, TermEvent};

/// 监听端口失败时的重试次数（每次间隔 300ms）。
const BIND_RETRIES: u32 = 10;

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const OPT_BINARY: u8 = 0;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;

/// 连接建立时服务端发出的协商：由服务端（设备）回显、不使用 Go-Ahead、双向二进制传输。
/// 这样终端会进入逐字符模式，不做本地回显和行缓冲。
const NEGOTIATION: [u8; 12] = [
    IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SGA, IAC, WILL, OPT_BINARY, IAC, DO, OPT_BINARY,
];

pub async fn serve(hub: Arc<PortHub>, bind: String) {
    let addr = format!("{bind}:{}", hub.config().tcp_port);
    // 重新加载配置时，旧的监听任务刚被取消，端口可能还没释放，所以重试几次。
    let mut attempt = 0;
    let listener = loop {
        match TcpListener::bind(&addr).await {
            Ok(l) => break l,
            Err(e) if attempt < BIND_RETRIES => {
                attempt += 1;
                tracing::debug!(port = %hub.name(), "监听 {addr} 失败，稍后重试: {e}");
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => {
                tracing::error!(port = %hub.name(), "终端服务监听 {addr} 失败: {e}");
                return;
            }
        }
    };
    tracing::info!(port = %hub.name(), "终端服务监听 {addr}（{:?}）", hub.config().mode);
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(handle_client(hub.clone(), stream, peer));
            }
            Err(e) => tracing::warn!(port = %hub.name(), "接受连接失败: {e}"),
        }
    }
}

async fn handle_client(hub: Arc<PortHub>, stream: TcpStream, peer: SocketAddr) {
    let _guard = hub.client_guard();
    let _ = stream.set_nodelay(true);
    let telnet = hub.config().mode == TerminalMode::Telnet;
    tracing::info!(port = %hub.name(), "终端已连接: {peer}");

    // 先订阅再发欢迎信息，避免漏掉中间到达的数据。
    let mut events = hub.subscribe_terminal();
    let (mut reader, mut writer) = stream.into_split();

    let mut hello = Vec::new();
    if telnet {
        hello.extend_from_slice(&NEGOTIATION);
    }
    let encoding = hub.config().encoding;
    hello.extend_from_slice(&notice(&banner(&hub), encoding));
    if writer.write_all(&hello).await.is_err() {
        return;
    }

    // 协商回复也要经由发送任务写出，保证和数据不交错。
    let (reply_tx, mut reply_rx) = mpsc::channel::<Vec<u8>>(64);
    let name = hub.name().to_string();
    let mut outbound = tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                reply = reply_rx.recv() => match reply {
                    // 协商命令原样发出，不能再做 IAC 转义
                    Some(bytes) => {
                        if writer.write_all(&bytes).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    None => break,
                },
                event = events.recv() => event,
            };
            let bytes = match event {
                Ok(TermEvent::Data(data)) if telnet => escape_iac(&data),
                Ok(TermEvent::Data(data)) => data.to_vec(),
                Ok(TermEvent::Notice(text)) => notice(&text, encoding),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(port = %name, "终端 {peer} 处理过慢，丢弃 {n} 段数据");
                    notice(&format!("连接过慢，丢失了 {n} 段数据"), encoding)
                }
                Err(RecvError::Closed) => break,
            };
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut parser = TelnetParser::default();
    let mut buf = vec![0u8; 4096];
    let mut closed = hub.closed();
    loop {
        tokio::select! {
            _ = &mut outbound => break,
            _ = closed.wait_for(|c| *c) => break,
            read = reader.read(&mut buf) => {
                let n = match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if telnet {
                    let (data, replies) = parser.feed(&buf[..n]);
                    hub.write(data);
                    // 正常客户端只在连接开始时协商几次；回复积压说明对端不读数据，直接断开。
                    if !replies.is_empty() && reply_tx.try_send(replies).is_err() {
                        tracing::warn!(port = %hub.name(), "终端 {peer} 协商回复积压，断开连接");
                        break;
                    }
                } else {
                    hub.write(&buf[..n]);
                }
            }
        }
    }
    outbound.abort();
    tracing::info!(port = %hub.name(), "终端已断开: {peer}");
}

fn banner(hub: &PortHub) -> String {
    let state = match hub.state() {
        PortState::Connected { com } => format!("{com}，{} 8N1", hub.config().baud),
        PortState::Waiting => "等待设备接入".into(),
        PortState::Error(e) => e,
    };
    format!("{}（{state}）", hub.name())
}

/// 服务提示单独占一行，前后换行，避免和设备输出混在同一行。
///
/// 按串口的编码输出：终端的编码是按设备输出设置的（例如 XShell 默认 GBK），提示要和它一致。
fn notice(text: &str, encoding: Encoding) -> Vec<u8> {
    let line = format!("\r\n[uarttool] {text}\r\n");
    match encoding {
        Encoding::Utf8 => line.into_bytes(),
        Encoding::Gbk => encoding_rs::GBK.encode(&line).0.into_owned(),
    }
}

/// Telnet 数据中的 0xFF 要转义为 0xFF 0xFF。
fn escape_iac(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    for &b in data {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
    out
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    #[default]
    Data,
    /// 收到 `\r`，下一个 `\0` 要丢掉
    Cr,
    Iac,
    /// 收到 WILL/WONT/DO/DONT，等待选项字节
    Option(u8),
    Sub,
    SubIac,
}

/// 从客户端数据中去掉 Telnet 命令，并对客户端的协商请求生成回复。
#[derive(Debug, Default)]
struct TelnetParser {
    state: ParseState,
}

impl TelnetParser {
    /// 返回（要写给串口的数据，要回复给客户端的协商）。
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut data = Vec::with_capacity(input.len());
        let mut replies = Vec::new();
        for &b in input {
            self.state = match (self.state, b) {
                (ParseState::Data | ParseState::Cr, IAC) => ParseState::Iac,
                (ParseState::Cr, 0) => ParseState::Data,
                (ParseState::Data | ParseState::Cr, b'\r') => {
                    data.push(b);
                    ParseState::Cr
                }
                (ParseState::Data | ParseState::Cr, _) => {
                    data.push(b);
                    ParseState::Data
                }
                (ParseState::Iac, IAC) => {
                    data.push(IAC);
                    ParseState::Data
                }
                (ParseState::Iac, WILL | WONT | DO | DONT) => ParseState::Option(b),
                (ParseState::Iac, SB) => ParseState::Sub,
                (ParseState::Iac, _) => ParseState::Data,
                (ParseState::Option(verb), opt) => {
                    replies.extend_from_slice(&reply(verb, opt));
                    ParseState::Data
                }
                (ParseState::Sub, IAC) => ParseState::SubIac,
                (ParseState::Sub, _) => ParseState::Sub,
                (ParseState::SubIac, SE) => ParseState::Data,
                (ParseState::SubIac, _) => ParseState::Sub,
            };
        }
        (data, replies)
    }
}

/// 对客户端协商的回复。我们主动提出的选项（ECHO/SGA/BINARY）客户端同意时不再回复，
/// 其余选项一律拒绝；对 WONT/DONT 不回复。这样不会出现来回协商的死循环。
fn reply(verb: u8, opt: u8) -> Vec<u8> {
    match verb {
        DO if matches!(opt, OPT_ECHO | OPT_SGA | OPT_BINARY) => vec![],
        DO => vec![IAC, WONT, opt],
        WILL if matches!(opt, OPT_SGA | OPT_BINARY) => vec![],
        WILL => vec![IAC, DONT, opt],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_data() {
        let mut p = TelnetParser::default();
        assert_eq!(p.feed(b"reboot\r\n"), (b"reboot\r\n".to_vec(), vec![]));
    }

    #[test]
    fn strips_cr_nul() {
        let mut p = TelnetParser::default();
        assert_eq!(p.feed(b"ls\r\0").0, b"ls\r");
        // \r\0 分在两次读取里
        assert_eq!(p.feed(b"ps\r").0, b"ps\r");
        assert_eq!(p.feed(b"\0x").0, b"x");
    }

    #[test]
    fn unescapes_iac_and_drops_commands() {
        let mut p = TelnetParser::default();
        let (data, replies) = p.feed(&[b'a', IAC, IAC, b'b', IAC, 241, b'c']);
        assert_eq!(data, vec![b'a', IAC, b'b', b'c']);
        assert!(replies.is_empty());
    }

    #[test]
    fn answers_negotiation() {
        let mut p = TelnetParser::default();
        // 客户端同意我们的 ECHO/BINARY，要求 NAWS(31)、提出 TTYPE(24)
        let input = [IAC, DO, OPT_ECHO, IAC, WILL, OPT_BINARY, IAC, WILL, 31, IAC, DO, 24, b'x'];
        let (data, replies) = p.feed(&input);
        assert_eq!(data, b"x");
        assert_eq!(replies, vec![IAC, DONT, 31, IAC, WONT, 24]);
    }

    #[test]
    fn skips_subnegotiation_across_reads() {
        let mut p = TelnetParser::default();
        assert_eq!(p.feed(&[b'a', IAC, SB, 31, 0, 80]).0, b"a");
        assert_eq!(p.feed(&[0, 24, IAC, SE, b'b']).0, b"b");
    }

    #[test]
    fn escapes_outgoing_iac() {
        assert_eq!(escape_iac(&[1, IAC, 2]), vec![1, IAC, IAC, 2]);
    }
}
