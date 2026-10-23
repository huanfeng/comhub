//! 单个串口的数据中枢：一个 IO 线程独占物理串口，把数据分发给终端、环形缓冲和日志文件。

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Local;
use serde::Serialize;
use serialport::SerialPort;
use tokio::sync::{broadcast, watch};

use crate::buffer::{Direction, LogBuffer, LogLine};
use crate::config::{PortConfig, TerminalMode};
use crate::devices::{self, find_device};
use crate::logfile::LogFile;
use crate::text::{render_line, LineSplitter, SplitMode};

/// 串口读超时。同时决定了写入的最大延迟和空闲切行的检查粒度。
const READ_TIMEOUT: Duration = Duration::from_millis(20);
/// 等待设备出现时的轮询间隔。
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// 已连接时，多久确认一次设备还在（有些驱动在拔出后读操作只是超时，不报错）。
const PRESENCE_CHECK_INTERVAL: Duration = Duration::from_secs(3);
/// 等待写出的数据上限；超过后暂不从队列取新数据。
const MAX_PENDING_WRITE: usize = 64 * 1024;
/// 设备未连接时"丢弃写入"提示的最小间隔。
const DROP_NOTICE_INTERVAL: Duration = Duration::from_secs(5);

/// 推送给终端连接的事件。
#[derive(Debug, Clone)]
pub enum TermEvent {
    /// 来自设备的原始字节
    Data(Bytes),
    /// 服务提示（连接、断开等），由终端层格式化后显示
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum PortState {
    /// 设备未接入，正在等待
    Waiting,
    /// 已打开
    Connected { com: String },
    /// 设备存在但打不开（例如被其它程序占用）
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct PortSnapshot {
    pub name: String,
    pub state: PortState,
    pub baud: u32,
    pub tcp_port: u16,
    pub mode: TerminalMode,
    pub clients: usize,
    pub last_seq: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub struct PortHub {
    config: PortConfig,
    state: Mutex<PortState>,
    buffer: Arc<Mutex<LogBuffer>>,
    term_tx: broadcast::Sender<TermEvent>,
    line_tx: broadcast::Sender<LogLine>,
    write_tx: mpsc::Sender<Vec<u8>>,
    clients: AtomicUsize,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    stop: AtomicBool,
    /// 停止时置为 true，让终端连接和 HTTP 流随之结束。
    closed: watch::Sender<bool>,
    on_change: Box<dyn Fn(&str) + Send + Sync>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// 启动一个串口所需的、来自上一代同名串口的东西。
#[derive(Default)]
pub struct Handover {
    /// 沿用旧的环形缓冲：seq 继续递增，调用方手里的游标不会失效。
    pub buffer: Option<Arc<Mutex<LogBuffer>>>,
    /// 旧的 IO 线程；新线程先等它关闭串口。
    pub predecessor: Option<JoinHandle<()>>,
}

impl PortHub {
    /// 创建并启动 IO 线程。`on_change` 在状态或客户端数量变化时调用，参数是串口别名。
    pub fn start(
        config: PortConfig,
        buffer_lines: usize,
        log_root: &Path,
        handover: Handover,
        on_change: impl Fn(&str) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let (write_tx, write_rx) = mpsc::channel();
        let buffer = match handover.buffer {
            Some(buffer) => {
                buffer.lock().unwrap().set_capacity(buffer_lines);
                buffer
            }
            None => Arc::new(Mutex::new(LogBuffer::new(buffer_lines))),
        };
        let hub = Arc::new(Self {
            state: Mutex::new(PortState::Waiting),
            buffer,
            term_tx: broadcast::channel(1024).0,
            line_tx: broadcast::channel(4096).0,
            write_tx,
            clients: AtomicUsize::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            closed: watch::channel(false).0,
            on_change: Box::new(on_change),
            thread: Mutex::new(None),
            config,
        });
        let log = LogFile::new(log_root, &hub.config.name);
        let worker = Worker {
            hub: hub.clone(),
            write_rx,
            log,
            rx: LineSplitter::new(SplitMode::Rx, hub.config.encoding),
            tx: LineSplitter::new(SplitMode::Tx, hub.config.encoding),
            predecessor: handover.predecessor,
            pending: Vec::new(),
            dropped: 0,
            last_drop_notice: None,
        };
        let handle = std::thread::Builder::new()
            .name(format!("port-{}", hub.config.name))
            .spawn(move || worker.run())
            .expect("创建串口线程失败");
        *hub.thread.lock().unwrap() = Some(handle);
        hub
    }

    /// 通知 IO 线程停止并关闭终端连接，不等待；返回线程句柄，由调用方决定是否等待。
    ///
    /// IO 线程可能正卡在打开串口或枚举设备上（蓝牙串口可能要十几秒），
    /// 所以不要在 UI 线程或持有锁时等待它。
    pub fn request_stop(&self) -> Option<JoinHandle<()>> {
        self.stop.store(true, Ordering::SeqCst);
        self.closed.send_replace(true);
        self.thread.lock().unwrap().take()
    }

    /// 停止并等待 IO 线程退出（程序退出时使用）。
    pub fn stop(&self) {
        if let Some(handle) = self.request_stop() {
            let _ = handle.join();
        }
    }

    /// 环形缓冲的句柄，同名串口重建时交给新的 hub。
    pub fn buffer_handle(&self) -> Arc<Mutex<LogBuffer>> {
        self.buffer.clone()
    }

    pub fn config(&self) -> &PortConfig {
        &self.config
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn state(&self) -> PortState {
        self.state.lock().unwrap().clone()
    }

    pub fn snapshot(&self) -> PortSnapshot {
        PortSnapshot {
            name: self.config.name.clone(),
            state: self.state(),
            baud: self.config.baud,
            tcp_port: self.config.tcp_port,
            mode: self.config.mode,
            clients: self.clients.load(Ordering::Relaxed),
            last_seq: self.buffer.lock().unwrap().last_seq(),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
        }
    }

    /// 写往设备。设备未连接时数据会被丢弃，并在日志中记一条 SYS。
    pub fn write(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if !data.is_empty() {
            let _ = self.write_tx.send(data);
        }
    }

    /// 串口停止（被删除或配置变化）时变为 true。
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    pub fn subscribe_terminal(&self) -> broadcast::Receiver<TermEvent> {
        self.term_tx.subscribe()
    }

    pub fn subscribe_lines(&self) -> broadcast::Receiver<LogLine> {
        self.line_tx.subscribe()
    }

    /// 访问环形缓冲。闭包里不要做耗时操作，IO 线程写入时也要拿这把锁。
    pub fn with_buffer<R>(&self, f: impl FnOnce(&LogBuffer) -> R) -> R {
        f(&self.buffer.lock().unwrap())
    }

    pub fn with_buffer_mut<R>(&self, f: impl FnOnce(&mut LogBuffer) -> R) -> R {
        f(&mut self.buffer.lock().unwrap())
    }

    /// 登记一个终端连接，返回的守卫在 drop 时自动注销。
    pub fn client_guard(self: &Arc<Self>) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::Relaxed);
        (self.on_change)(&self.config.name);
        ClientGuard { hub: self.clone() }
    }

    fn set_state(&self, state: PortState) {
        let mut current = self.state.lock().unwrap();
        if *current != state {
            *current = state;
            drop(current);
            (self.on_change)(&self.config.name);
        }
    }
}

pub struct ClientGuard {
    hub: Arc<PortHub>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.hub.clients.fetch_sub(1, Ordering::Relaxed);
        (self.hub.on_change)(&self.hub.config.name);
    }
}

/// 只在 IO 线程中使用的状态。
struct Worker {
    hub: Arc<PortHub>,
    write_rx: mpsc::Receiver<Vec<u8>>,
    log: LogFile,
    rx: LineSplitter,
    tx: LineSplitter,
    predecessor: Option<JoinHandle<()>>,
    /// 已连接时等待写出的数据（写超时或部分写入时留到下一轮）
    pending: Vec<u8>,
    /// 未连接期间累计丢弃、尚未提示的写入字节数
    dropped: usize,
    last_drop_notice: Option<Instant>,
}

impl Worker {
    fn run(mut self) {
        // 同名串口重建时，等旧线程把串口关掉再打开，否则会先报一次"拒绝访问"。
        if let Some(previous) = self.predecessor.take() {
            let _ = previous.join();
        }
        let mut last_error: Option<String> = None;
        let mut announced_waiting = false;
        while !self.stopped() {
            let devices = devices::list_devices();
            let Some(device) = find_device(&self.hub.config, &devices).cloned() else {
                if !announced_waiting {
                    self.sys(format!("等待设备接入（{}）", self.match_rule()));
                    announced_waiting = true;
                }
                self.hub.set_state(PortState::Waiting);
                last_error = None;
                self.idle(SCAN_INTERVAL);
                continue;
            };

            match serialport::new(&device.com, self.hub.config.baud)
                .timeout(READ_TIMEOUT)
                .open()
            {
                Ok(port) => {
                    announced_waiting = false;
                    last_error = None;
                    // 断开期间还没来得及提示的丢弃字节，在连接前报出来，不要算到下一次断开头上
                    self.report_dropped();
                    self.hub.set_state(PortState::Connected { com: device.com.clone() });
                    self.sys(format!(
                        "已连接 {}（{} 8N1）",
                        device.com, self.hub.config.baud
                    ));
                    let reason = self.serve(port, &device.com);
                    if !self.pending.is_empty() {
                        let n = self.pending.len();
                        self.pending.clear();
                        self.sys(format!("断开时有 {n} 字节没来得及写出，已丢弃"));
                    }
                    self.flush_partial();
                    if self.stopped() {
                        break;
                    }
                    self.sys(format!("设备已断开：{reason}"));
                    self.hub.set_state(PortState::Waiting);
                }
                Err(e) => {
                    let msg = format!("打开 {} 失败：{e}", device.com);
                    if last_error.as_ref() != Some(&msg) {
                        self.sys(msg.clone());
                        last_error = Some(msg.clone());
                    }
                    self.hub.set_state(PortState::Error(msg));
                    self.idle(SCAN_INTERVAL * 2);
                }
            }
        }
        self.flush_partial();
        self.log.flush();
    }

    /// 已连接状态下的读写循环；返回断开的原因。
    fn serve(&mut self, mut port: Box<dyn SerialPort>, com: &str) -> String {
        let mut buf = vec![0u8; 4096];
        let mut last_presence_check = Instant::now();
        loop {
            if self.stopped() {
                return "服务停止".into();
            }
            match port.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => self.on_rx(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return e.to_string(),
            }

            // 写超时（低波特率或驱动发送缓冲满）不是断开：没写完的留到下一轮继续写。
            while self.pending.len() < MAX_PENDING_WRITE {
                match self.write_rx.try_recv() {
                    Ok(data) => self.pending.extend_from_slice(&data),
                    Err(_) => break,
                }
            }
            if !self.pending.is_empty() {
                match port.write(&self.pending) {
                    Ok(n) => {
                        let written: Vec<u8> = self.pending.drain(..n).collect();
                        self.on_tx(&written);
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                    Err(e) => return e.to_string(),
                }
            }

            let now = Instant::now();
            if let Some(line) = self.rx.poll_idle(now) {
                self.emit(Direction::Rx, &line);
            }
            if let Some(line) = self.tx.poll_idle(now) {
                self.emit(Direction::Tx, &line);
            }
            self.log.flush();

            if now.duration_since(last_presence_check) >= PRESENCE_CHECK_INTERVAL {
                last_presence_check = now;
                if !devices::list_devices().iter().any(|d| d.com.eq_ignore_ascii_case(com)) {
                    return "设备已移除".into();
                }
            }
        }
    }

    fn on_rx(&mut self, data: &[u8]) {
        self.hub.rx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        let _ = self.hub.term_tx.send(TermEvent::Data(Bytes::copy_from_slice(data)));
        for line in self.rx.feed(data, Instant::now()) {
            self.emit(Direction::Rx, &line);
        }
    }

    fn on_tx(&mut self, data: &[u8]) {
        self.hub.tx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        for line in self.tx.feed(data, Instant::now()) {
            self.emit(Direction::Tx, &line);
        }
    }

    fn emit(&mut self, dir: Direction, raw: &[u8]) {
        let text = render_line(raw, self.hub.config.encoding);
        self.push(dir, text);
    }

    fn sys(&mut self, text: String) {
        tracing::info!(port = %self.hub.config.name, "{text}");
        let _ = self.hub.term_tx.send(TermEvent::Notice(text.clone()));
        self.push(Direction::Sys, text);
        self.log.flush();
    }

    fn push(&mut self, dir: Direction, text: String) {
        let line = self.hub.buffer.lock().unwrap().push(Local::now(), dir, text);
        self.log.write(&line);
        let _ = self.hub.line_tx.send(line);
    }

    /// 断开时把两个方向未结束的内容写出，避免和下一次连接的数据拼在一起。
    fn flush_partial(&mut self) {
        if let Some(line) = self.rx.take() {
            self.emit(Direction::Rx, &line);
        }
        if let Some(line) = self.tx.take() {
            self.emit(Direction::Tx, &line);
        }
    }

    /// 未连接时等待一段时间；期间到达的写入会被丢弃。
    ///
    /// 丢弃提示合并后最多每 5 秒一条，否则终端里每敲一个键都会刷出一行。
    fn idle(&mut self, total: Duration) {
        let deadline = Instant::now() + total;
        while !self.stopped() && Instant::now() < deadline {
            self.dropped += self.write_rx.try_iter().map(|d| d.len()).sum::<usize>();
            let due = self
                .last_drop_notice
                .is_none_or(|t| t.elapsed() >= DROP_NOTICE_INTERVAL);
            if due {
                self.report_dropped();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn report_dropped(&mut self) {
        if self.dropped > 0 {
            self.sys(format!("设备未连接，丢弃 {} 字节写入", self.dropped));
            self.dropped = 0;
            self.last_drop_notice = Some(Instant::now());
        }
    }

    fn stopped(&self) -> bool {
        self.hub.stop.load(Ordering::SeqCst)
    }

    fn match_rule(&self) -> String {
        match (&self.hub.config.serial_number, &self.hub.config.com) {
            (Some(sn), _) => format!("序列号 {sn}"),
            (None, Some(com)) => com.clone(),
            (None, None) => "未配置".into(),
        }
    }
}
