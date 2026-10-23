//! HTTP 接口：给 AI、脚本和 uartctl 使用，默认返回纯文本。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::oneshot;

use crate::buffer::{Direction, LogLine};
use crate::devices;
use crate::port::{PortHub, PortSnapshot, PortState};
use crate::service::Service;

const USAGE: &str = include_str!("usage.md");
const MAX_TIMEOUT_SECS: f64 = 600.0;
/// 监听端口失败时的重试次数（每次间隔 300ms）。
const BIND_RETRIES: u32 = 10;
/// wait / expect 期间最多返回的行数（保留最新的）。
const MAX_WAIT_LINES: usize = 5000;

pub fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/", get(usage))
        .route("/ports", get(list_ports))
        .route("/devices", get(list_devices))
        .route("/ports/{name}/tail", get(tail))
        .route("/ports/{name}/log", get(log))
        .route("/ports/{name}/grep", get(grep))
        .route("/ports/{name}/wait", get(wait))
        .route("/ports/{name}/stream", get(stream))
        .route("/ports/{name}/send", post(send))
        .layer(middleware::from_fn_with_state(service.clone(), auth))
        .with_state(service)
}

/// 运行 HTTP 服务，直到 `stop` 收到信号（或发送端被丢弃）后优雅关闭。
pub async fn serve(service: Arc<Service>, bind: String, port: u16, stop: oneshot::Receiver<()>) {
    let addr = format!("{bind}:{port}");
    // 重新加载配置时旧服务刚收到关闭信号，端口可能还没释放（Windows 上不设 SO_REUSEADDR），所以重试几次。
    let mut attempt = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => break l,
            Err(e) if attempt < BIND_RETRIES => {
                attempt += 1;
                tracing::debug!("HTTP 监听 {addr} 失败，稍后重试: {e}");
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => {
                tracing::error!("HTTP 服务监听 {addr} 失败: {e}");
                return;
            }
        }
    };
    tracing::info!("HTTP 服务监听 http://{addr}/");
    let shutdown = async {
        let _ = stop.await;
    };
    if let Err(e) = axum::serve(listener, router(service))
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!("HTTP 服务异常退出: {e}");
    }
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    format: Option<String>,
    lines: Option<usize>,
    since: Option<u64>,
    limit: Option<usize>,
    pattern: Option<String>,
    expect: Option<String>,
    timeout: Option<f64>,
    eol: Option<String>,
    dir: Option<String>,
    token: Option<String>,
}

impl Params {
    fn json(&self) -> bool {
        self.format.as_deref() == Some("json")
    }

    fn timeout(&self, default_secs: f64) -> Duration {
        // NaN / inf 会让 Duration::from_secs_f64 panic
        let secs = self.timeout.filter(|t| t.is_finite()).unwrap_or(default_secs);
        Duration::from_secs_f64(secs.clamp(0.0, MAX_TIMEOUT_SECS))
    }

    /// `dir=any` 时 TX/SYS 行也参与 wait / expect 的匹配。
    fn any_dir(&self) -> bool {
        self.dir.as_deref() == Some("any")
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, format!("错误: {}\n", self.1)).into_response()
    }
}

type ApiResult = Result<Response, ApiError>;

async fn auth(
    State(service): State<Arc<Service>>,
    Query(params): Query<Params>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // 防止本机浏览器里的网页借道访问：浏览器的跨域请求总会带 Origin，命令行工具不会；
    // DNS 重绑定攻击的 Host 是攻击者的域名，这里只接受 IP、本机名和 .local 名称。
    if headers.contains_key(header::ORIGIN) {
        return ApiError(StatusCode::FORBIDDEN, "不接受来自浏览器网页的请求".into()).into_response();
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !is_allowed_host(host) {
        return ApiError(
            StatusCode::FORBIDDEN,
            format!("不接受 Host `{host}`：请用 IP 地址、localhost 或不带点的主机名访问"),
        )
        .into_response();
    }

    if let Some(token) = service.config().server.token {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if bearer != Some(token.as_str()) && params.token.as_deref() != Some(token.as_str()) {
            return ApiError(
                StatusCode::UNAUTHORIZED,
                "需要 token：请求头 `Authorization: Bearer <token>` 或参数 `?token=<token>`".into(),
            )
            .into_response();
        }
    }
    next.run(request).await
}

/// Host 头（可带端口）是否是 IP、`localhost`、不带点的主机名或 `*.local`。
fn is_allowed_host(host: &str) -> bool {
    let name = if let Some(v6) = host.strip_prefix('[') {
        // [::1]:7000
        return v6.split(']').next().is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok());
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    if name.is_empty() {
        return false;
    }
    let name = name.to_ascii_lowercase();
    name.parse::<std::net::Ipv4Addr>().is_ok()
        || name == "localhost"
        || !name.contains('.')
        || name.ends_with(".local")
}

fn find_port(service: &Service, name: &str) -> Result<Arc<PortHub>, ApiError> {
    service.port(name).ok_or_else(|| {
        let names: Vec<String> = service.ports().iter().map(|h| h.name().to_string()).collect();
        ApiError(
            StatusCode::NOT_FOUND,
            format!("未找到串口 `{name}`，可用的串口：{}", names.join(", ")),
        )
    })
}

fn parse_regex(pattern: Option<&str>, field: &str) -> Result<Regex, ApiError> {
    let pattern = pattern
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("缺少参数 `{field}`")))?;
    Regex::new(pattern)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("`{field}` 不是合法的正则: {e}")))
}

fn text(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

fn json(value: &impl Serialize) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(value).unwrap_or_default(),
    )
        .into_response()
}

fn render_lines(lines: &[LogLine]) -> String {
    let mut out = String::new();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    out
}

fn lines_response(lines: &[LogLine], params: &Params) -> Response {
    if params.json() {
        json(&lines)
    } else {
        text(render_lines(lines))
    }
}

fn state_text(state: &PortState) -> String {
    match state {
        PortState::Waiting => "waiting".into(),
        PortState::Connected { com } => format!("connected({com})"),
        PortState::Error(e) => format!("error: {e}"),
    }
}

fn ports_table(ports: &[PortSnapshot]) -> String {
    let mut out = format!(
        "{:<16} {:<20} {:>7} {:>5} {:<6} {:>7} {:>9}\n",
        "NAME", "STATE", "BAUD", "TCP", "MODE", "CLIENTS", "LAST_SEQ"
    );
    for p in ports {
        let mode = format!("{:?}", p.mode).to_lowercase();
        let _ = writeln!(
            out,
            "{:<16} {:<20} {:>7} {:>5} {:<6} {:>7} {:>9}",
            p.name,
            state_text(&p.state),
            p.baud,
            p.tcp_port,
            mode,
            p.clients,
            p.last_seq
        );
    }
    out
}

async fn usage(State(service): State<Arc<Service>>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:7000");
    let mut doc = USAGE.replace("{base}", &format!("http://{host}"));
    let _ = write!(doc, "\n## 当前串口\n\n```\n{}```\n", ports_table(&service.snapshots()));
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], doc).into_response()
}

async fn list_ports(State(service): State<Arc<Service>>, Query(params): Query<Params>) -> Response {
    let ports = service.snapshots();
    if params.json() {
        json(&ports)
    } else {
        text(ports_table(&ports))
    }
}

async fn list_devices(Query(params): Query<Params>) -> Response {
    let devices = tokio::task::spawn_blocking(devices::list_devices)
        .await
        .unwrap_or_default();
    if params.json() {
        return json(&devices);
    }
    let mut out = format!("{:<8} {:<10} {:<20} {}\n", "COM", "VID:PID", "SERIAL", "DESCRIPTION");
    for d in devices {
        let vid_pid = match (d.vid, d.pid) {
            (Some(v), Some(p)) => format!("{v:04X}:{p:04X}"),
            _ => "-".into(),
        };
        let _ = writeln!(
            out,
            "{:<8} {:<10} {:<20} {}",
            d.com,
            vid_pid,
            d.serial_number.as_deref().unwrap_or("-"),
            d.description.as_deref().unwrap_or("")
        );
    }
    text(out)
}

async fn tail(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let lines = hub.with_buffer(|b| b.tail(params.lines.unwrap_or(200)));
    Ok(lines_response(&lines, &params))
}

async fn log(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let (lines, last_seq) = hub.with_buffer(|b| {
        (b.since(params.since.unwrap_or(0), params.limit.unwrap_or(1000)), b.last_seq())
    });
    let since = params.since.unwrap_or(0);
    // 没有新数据时游标保持不变；有数据时指向本次最后一行（可能还没读完，下次接着读）。
    let next = lines.last().map(|l| l.seq).unwrap_or_else(|| since.min(last_seq));
    // 游标之后的部分行已被挤出缓冲：告诉调用方中间有缺口，而不是悄悄跳过。
    let gap = lines
        .first()
        .filter(|first| since > 0 && first.seq > since + 1)
        .map(|first| (since + 1, first.seq - 1));
    let mut response = if params.json() {
        #[derive(Serialize)]
        struct LogPage<'a> {
            lines: &'a [LogLine],
            next_seq: u64,
            gap: Option<(u64, u64)>,
        }
        json(&LogPage { lines: &lines, next_seq: next, gap })
    } else {
        let mut body = String::new();
        if let Some((from, to)) = gap {
            let _ = writeln!(body, "-- gap: 第 {from}..{to} 行已不在内存缓冲中，完整内容见日志文件");
        }
        body.push_str(&render_lines(&lines));
        let _ = writeln!(body, "-- next_seq={next}");
        text(body)
    };
    response
        .headers_mut()
        .insert("x-next-seq", HeaderValue::from(next));
    Ok(response)
}

async fn grep(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let re = parse_regex(params.pattern.as_deref(), "pattern")?;
    let lines = hub.with_buffer(|b| b.grep(&re, params.limit.unwrap_or(200)));
    Ok(lines_response(&lines, &params))
}

async fn wait(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let re = parse_regex(params.pattern.as_deref(), "pattern")?;
    let baseline = hub.with_buffer(|b| b.last_seq());
    let mut rx = hub.subscribe_lines();
    let (lines, end) =
        collect_until(&hub, &mut rx, &re, params.any_dir(), params.timeout(30.0), baseline).await;
    Ok(wait_response(&lines, end, &params))
}

async fn send(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
    body: Bytes,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let expect = match params.expect.as_deref() {
        Some(_) => Some(parse_regex(params.expect.as_deref(), "expect")?),
        None => None,
    };
    let eol: &[u8] = match params.eol.as_deref().unwrap_or("crlf") {
        "crlf" => b"\r\n",
        "cr" => b"\r",
        "lf" => b"\n",
        "none" => b"",
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("eol 只能是 crlf/cr/lf/none，收到 `{other}`"),
            ))
        }
    };
    if !matches!(hub.state(), PortState::Connected { .. }) {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("串口 `{}` 未连接（{}）", hub.name(), state_text(&hub.state())),
        ));
    }

    // 先取基准、再订阅、最后写入，避免设备回复得太快而漏掉。
    let baseline = hub.with_buffer(|b| b.last_seq());
    let mut rx = hub.subscribe_lines();
    let mut data = body.to_vec();
    data.extend_from_slice(eol);
    let len = data.len();
    hub.write(data);

    match expect {
        Some(re) => {
            let (lines, end) =
                collect_until(&hub, &mut rx, &re, params.any_dir(), params.timeout(5.0), baseline).await;
            Ok(wait_response(&lines, end, &params))
        }
        None if params.json() => Ok(json(&serde_json::json!({ "sent": len }))),
        None => Ok(text(format!("已发送 {len} 字节\n"))),
    }
}

async fn stream(
    State(service): State<Arc<Service>>,
    Path(name): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult {
    let hub = find_port(&service, &name)?;
    let json_lines = params.json();
    let state = (hub.subscribe_lines(), hub.closed());
    let body = futures_util::stream::unfold(state, move |(mut rx, mut closed)| async move {
        let received = tokio::select! {
            r = rx.recv() => r,
            _ = closed.wait_for(|c| *c) => return None,
        };
        let chunk = match received {
            Ok(line) if json_lines => format!("{}\n", serde_json::to_string(&line).unwrap_or_default()),
            Ok(line) => format!("{line}\n"),
            Err(RecvError::Lagged(n)) => format!("-- lagged: 丢失 {n} 行\n"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok::<_, std::convert::Infallible>(chunk), (rx, closed)))
    });
    let content_type = if json_lines {
        "application/x-ndjson"
    } else {
        "text/plain; charset=utf-8"
    };
    Ok(([(header::CONTENT_TYPE, content_type)], Body::from_stream(body)).into_response())
}

/// 等待结束的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitEnd {
    Matched,
    Timeout,
    /// 串口被停止（配置被修改或删除）
    Closed,
}

/// 收集新到达的行，直到出现匹配行、超时或串口停止。
///
/// 默认只拿设备输出（RX）去匹配：发出去的命令本身（TX）常常包含要等的关键字，
/// 比如 `send reboot --expect boot`。`any_dir` 为真时 TX/SYS 行也参与匹配。
/// 返回的行包含所有方向，方便看清上下文。
///
/// `last_seen` 是开始等待时缓冲中最新的 seq，必须在订阅广播**之前**读取：之后的行既会从广播收到，
/// 也会在积压补齐时按 seq 去重，不会被误当成旧行跳过。
async fn collect_until(
    hub: &PortHub,
    rx: &mut broadcast::Receiver<LogLine>,
    re: &Regex,
    any_dir: bool,
    timeout: Duration,
    mut last_seen: u64,
) -> (Vec<LogLine>, WaitEnd) {
    let mut closed = hub.closed();
    let mut lines = VecDeque::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let received = tokio::select! {
            r = tokio::time::timeout_at(deadline, rx.recv()) => r,
            _ = closed.wait_for(|c| *c) => return (lines.into(), WaitEnd::Closed),
        };
        let batch = match received {
            Err(_) => return (lines.into(), WaitEnd::Timeout),
            Ok(Err(RecvError::Closed)) => return (lines.into(), WaitEnd::Closed),
            // 积压时一次补齐到缓冲里最新的一行，中间的行都参与匹配（返回的行仍只保留最近 MAX_WAIT_LINES 条）
            Ok(Err(RecvError::Lagged(_))) => hub.with_buffer(|b| b.since(last_seen, usize::MAX)),
            Ok(Ok(line)) => vec![line],
        };
        for line in batch {
            if line.seq <= last_seen {
                continue;
            }
            last_seen = line.seq;
            let hit = (any_dir || line.dir == Direction::Rx) && re.is_match(&line.text);
            if lines.len() == MAX_WAIT_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
            if hit {
                return (lines.into(), WaitEnd::Matched);
            }
        }
    }
}

fn wait_response(lines: &[LogLine], end: WaitEnd, params: &Params) -> Response {
    let (status, note) = match end {
        WaitEnd::Matched => (StatusCode::OK, None),
        WaitEnd::Timeout => (StatusCode::REQUEST_TIMEOUT, Some("-- timeout: 未等到匹配行")),
        WaitEnd::Closed => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("-- closed: 串口已停止（配置被修改或删除）"),
        ),
    };
    let mut response = if params.json() {
        json(&serde_json::json!({ "matched": end == WaitEnd::Matched, "lines": lines }))
    } else {
        let mut body = render_lines(lines);
        if let Some(note) = note {
            body.push_str(note);
            body.push('\n');
        }
        text(body)
    };
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allowlist() {
        for ok in ["127.0.0.1:7000", "192.168.1.10", "localhost:7000", "devpc:7000", "devpc.local", "[::1]:7000"] {
            assert!(is_allowed_host(ok), "{ok}");
        }
        for bad in ["", "evil.com", "evil.com:7000", "127.0.0.1.nip.io", "[evil]:7000"] {
            assert!(!is_allowed_host(bad), "{bad}");
        }
    }
}
