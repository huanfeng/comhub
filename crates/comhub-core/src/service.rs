//! 服务入口：按配置启动所有串口，以及它们的终端服务和 HTTP 服务。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::config::{Config, PortConfig, ServerConfig};
use crate::port::{Handover, PortHub, PortSnapshot};
use crate::{http, terminal};

/// 通知外部（例如 GUI）刷新显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    /// 某个串口的状态或客户端数量变化
    PortChanged(String),
    /// 串口列表变化（重新加载配置后）
    PortsReloaded,
}

type EventSink = Arc<dyn Fn(ServiceEvent) + Send + Sync>;

struct PortEntry {
    hub: Arc<PortHub>,
    terminal: JoinHandle<()>,
}

pub struct Service {
    config: Mutex<Config>,
    config_path: PathBuf,
    ports: Mutex<BTreeMap<String, PortEntry>>,
    http: Mutex<Option<HttpServer>>,
    /// 已停止的串口留下的缓冲与线程句柄，按别名索引，供同名串口再次启动时接手
    retired: Mutex<BTreeMap<String, Handover>>,
    on_event: EventSink,
}

impl Service {
    /// 启动服务。需要在 tokio 运行时中调用。
    pub fn start(
        config: Config,
        config_path: &Path,
        on_event: impl Fn(ServiceEvent) + Send + Sync + 'static,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        let service = Arc::new(Self {
            config: Mutex::new(config.clone()),
            config_path: config_path.to_path_buf(),
            ports: Mutex::new(BTreeMap::new()),
            http: Mutex::new(None),
            retired: Mutex::new(BTreeMap::new()),
            on_event: Arc::new(on_event),
        });
        service.apply(None, config);
        Ok(service)
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn port(&self, name: &str) -> Option<Arc<PortHub>> {
        let ports = self.ports.lock().unwrap();
        ports
            .values()
            .find(|e| e.hub.name().eq_ignore_ascii_case(name))
            .map(|e| e.hub.clone())
    }

    pub fn ports(&self) -> Vec<Arc<PortHub>> {
        self.ports.lock().unwrap().values().map(|e| e.hub.clone()).collect()
    }

    pub fn snapshots(&self) -> Vec<PortSnapshot> {
        self.ports().iter().map(|h| h.snapshot()).collect()
    }

    /// 应用新配置：配置没变的串口保持运行（终端连接不断），其余的停止或新建。
    /// 需要在 tokio 运行时中调用。
    pub fn reload(self: &Arc<Self>, config: Config) -> Result<()> {
        config.validate()?;
        let old = std::mem::replace(&mut *self.config.lock().unwrap(), config.clone());
        self.apply(Some(&old.server), config);
        (self.on_event)(ServiceEvent::PortsReloaded);
        Ok(())
    }

    /// `old` 为 `None` 表示首次启动。
    fn apply(self: &Arc<Self>, old: Option<&ServerConfig>, config: Config) {
        let server = &config.server;
        // token 每个请求都现读配置，改它不需要重启任何东西。
        let http_changed = old.is_none_or(|o| o.bind != server.bind || o.http_port != server.http_port);
        // 终端端口跟随 bind，日志文件由 IO 线程持有，这两项变了要重建串口。
        let ports_changed = old.is_some_and(|o| o.bind != server.bind || o.log_dir != server.log_dir);

        if http_changed {
            warn_if_exposed(&config);
            let (stop_tx, stop_rx) = oneshot::channel();
            let task = tokio::spawn(http::serve(
                self.clone(),
                server.bind.clone(),
                server.http_port,
                stop_rx,
            ));
            if let Some(old) = self.http.lock().unwrap().replace(HttpServer { task, stop_tx }) {
                // 优雅关闭：旧监听立即停止接受连接，已建立的连接处理完当前请求后关闭。
                let _ = old.stop_tx.send(());
            }
        }

        let log_root = config.log_dir(&self.config_path);
        let wanted: BTreeMap<String, PortConfig> = config
            .ports
            .iter()
            .filter(|p| p.enabled)
            .map(|p| (p.name.clone(), p.clone()))
            .collect();

        // 锁内只摘出要停的条目，停止和等待都在锁外做。
        let stale: Vec<PortEntry> = {
            let mut ports = self.ports.lock().unwrap();
            let names: Vec<String> = ports
                .iter()
                .filter(|(name, entry)| ports_changed || wanted.get(*name) != Some(entry.hub.config()))
                .map(|(name, _)| name.clone())
                .collect();
            names.iter().filter_map(|n| ports.remove(n)).collect()
        };
        // 停下的串口留下缓冲和线程句柄：同名串口（这次重建，或以后停用后再启用、删掉后再加回）
        // 接着用同一份缓冲，seq 连续；并先等旧线程关闭串口再打开，避免"拒绝访问"。
        let mut retired = self.retired.lock().unwrap();
        for entry in stale {
            entry.terminal.abort();
            // 不在这里等待旧线程，由同名的新线程自己等。
            let predecessor = entry.hub.request_stop();
            retired.insert(
                entry.hub.name().to_string(),
                Handover { buffer: Some(entry.hub.buffer_handle()), predecessor },
            );
        }

        let mut ports = self.ports.lock().unwrap();
        for entry in ports.values() {
            entry.hub.with_buffer_mut(|b| b.set_capacity(server.buffer_lines));
        }
        for (name, port_config) in wanted {
            if ports.contains_key(&name) {
                continue;
            }
            let on_event = self.on_event.clone();
            let handover = retired.remove(&name).unwrap_or_default();
            let hub = PortHub::start(port_config, server.buffer_lines, &log_root, handover, move |n| {
                on_event(ServiceEvent::PortChanged(n.to_string()))
            });
            let terminal = tokio::spawn(terminal::serve(hub.clone(), server.bind.clone()));
            ports.insert(name, PortEntry { hub, terminal });
        }
    }

    /// 停止所有串口和 HTTP 服务，等待串口线程退出。
    pub fn shutdown(&self) {
        if let Some(http) = self.http.lock().unwrap().take() {
            http.task.abort();
        }
        let entries = std::mem::take(&mut *self.ports.lock().unwrap());
        let handles: Vec<_> = entries
            .into_values()
            .filter_map(|entry| {
                entry.terminal.abort();
                entry.hub.request_stop()
            })
            .collect();
        let retired = std::mem::take(&mut *self.retired.lock().unwrap());
        let retired_handles = retired.into_values().filter_map(|h| h.predecessor);
        for handle in handles.into_iter().chain(retired_handles) {
            let _ = handle.join();
        }
    }
}

struct HttpServer {
    task: JoinHandle<()>,
    stop_tx: oneshot::Sender<()>,
}
/// 对外监听时提醒当前的访问控制情况。
fn warn_if_exposed(config: &Config) {
    let loopback = config
        .server
        .bind
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    if loopback {
        return;
    }
    if config.server.token.is_none() {
        tracing::warn!(
            bind = %config.server.bind,
            "服务对局域网开放且未设置 token，任何能访问本机的设备都可以读写串口"
        );
    } else {
        tracing::warn!(
            bind = %config.server.bind,
            "token 只保护 HTTP 接口；终端端口（Telnet/Raw）不做认证，局域网内可以直接连接"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Encoding, ServerConfig, TerminalMode};

    fn port(name: &str, tcp_port: u16, baud: u32) -> PortConfig {
        PortConfig {
            name: name.into(),
            serial_number: None,
            // 不存在的串口：服务会一直处于等待状态，测试不依赖硬件
            com: Some("COM250".into()),
            baud,
            tcp_port,
            mode: TerminalMode::Telnet,
            encoding: Encoding::Utf8,
            enabled: true,
        }
    }

    fn config(ports: Vec<PortConfig>) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1".into(),
                http_port: 47100,
                ..ServerConfig::default()
            },
            ports,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_keeps_unchanged_ports_and_restarts_changed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let service = Service::start(config(vec![port("a", 47101, 115_200)]), &path, |_| {}).unwrap();
        let a = service.port("a").unwrap();

        service
            .reload(config(vec![port("a", 47101, 115_200), port("b", 47102, 115_200)]))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &service.port("A").unwrap()), "未变化的串口应保持运行");
        let b = service.port("b").unwrap();
        b.with_buffer_mut(|buf| {
            buf.push(chrono::Local::now(), crate::buffer::Direction::Rx, "x".into());
        });
        let seq_before = b.snapshot().last_seq;

        // 只改 token：串口和 HTTP 都不重启
        let mut with_token = config(vec![port("a", 47101, 115_200), port("b", 47102, 115_200)]);
        with_token.server.token = Some("t".into());
        service.reload(with_token).unwrap();
        assert!(Arc::ptr_eq(&b, &service.port("b").unwrap()), "只改 token 不应重建串口");

        service.reload(config(vec![port("b", 47102, 921_600)])).unwrap();
        assert!(service.port("a").is_none(), "删除的串口应停止");
        let b2 = service.port("b").unwrap();
        assert!(!Arc::ptr_eq(&b, &b2), "配置变化的串口应重新创建");
        assert_eq!(b2.config().baud, 921_600);
        assert!(Arc::ptr_eq(&b.buffer_handle(), &b2.buffer_handle()), "重建应沿用旧缓冲");
        assert!(b2.snapshot().last_seq >= seq_before, "重建后 seq 不应回退");
        assert!(*b.closed().borrow(), "旧的串口应发出关闭信号");

        let mut disabled = port("b", 47102, 921_600);
        disabled.enabled = false;
        service.reload(config(vec![disabled])).unwrap();
        assert!(service.ports().is_empty(), "停用的串口不应运行");

        // 停用后再启用：接着用原来的缓冲
        service.reload(config(vec![port("b", 47102, 921_600)])).unwrap();
        let b3 = service.port("b").unwrap();
        assert!(Arc::ptr_eq(&b2.buffer_handle(), &b3.buffer_handle()), "重新启用应沿用旧缓冲");

        assert!(service.reload(config(vec![port("x", 47100, 1)])).is_err(), "与 HTTP 端口冲突应拒绝");

        // 改 HTTP 端口：新端口要能连上（旧服务关闭后才能复用端口，这里换一个端口验证重启本身）
        let mut moved = config(vec![]);
        moved.server.http_port = 47110;
        service.reload(moved).unwrap();
        let mut connected = false;
        for _ in 0..20 {
            if tokio::net::TcpStream::connect("127.0.0.1:47110").await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(connected, "HTTP 服务应在新端口上监听");

        // 同一端口重启（改 bind 写法）：旧服务关闭、新服务重试 bind 后仍要在监听
        let mut rebind = config(vec![]);
        rebind.server.http_port = 47110;
        rebind.server.bind = "localhost".into();
        service.reload(rebind).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            tokio::net::TcpStream::connect("localhost:47110").await.is_ok(),
            "同端口重启后 HTTP 服务应继续监听"
        );
        service.shutdown();
    }
}
