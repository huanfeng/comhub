//! 服务入口：按配置启动所有串口，以及它们的终端服务和 HTTP 服务。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::config::{Config, PortConfig};
use crate::port::{PortHub, PortSnapshot};
use crate::terminal;

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
    tasks: Vec<JoinHandle<()>>,
}

pub struct Service {
    config: Mutex<Config>,
    config_path: PathBuf,
    ports: Mutex<BTreeMap<String, PortEntry>>,
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
        warn_if_exposed(&config);
        let service = Arc::new(Self {
            config: Mutex::new(Config::default()),
            config_path: config_path.to_path_buf(),
            ports: Mutex::new(BTreeMap::new()),
            on_event: Arc::new(on_event),
        });
        service.apply(config);
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
    pub fn reload(&self, config: Config) -> Result<()> {
        config.validate()?;
        self.apply(config);
        (self.on_event)(ServiceEvent::PortsReloaded);
        Ok(())
    }

    fn apply(&self, config: Config) {
        let old = std::mem::replace(&mut *self.config.lock().unwrap(), config.clone());
        let server_changed = old.server != config.server;
        let log_root = config.log_dir(&self.config_path);

        let mut ports = self.ports.lock().unwrap();
        let wanted: BTreeMap<String, PortConfig> = config
            .ports
            .iter()
            .filter(|p| p.enabled)
            .map(|p| (p.name.clone(), p.clone()))
            .collect();

        let stale: Vec<String> = ports
            .iter()
            .filter(|(name, entry)| {
                server_changed || wanted.get(*name) != Some(entry.hub.config())
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            if let Some(entry) = ports.remove(&name) {
                stop_entry(entry);
            }
        }

        for (name, port_config) in wanted {
            if ports.contains_key(&name) {
                continue;
            }
            let on_event = self.on_event.clone();
            let hub = PortHub::start(port_config, config.server.buffer_lines, &log_root, move |n| {
                on_event(ServiceEvent::PortChanged(n.to_string()))
            });
            let tasks = self.spawn_port_tasks(&config, &hub);
            ports.insert(name, PortEntry { hub, tasks });
        }
    }

    fn spawn_port_tasks(&self, config: &Config, hub: &Arc<PortHub>) -> Vec<JoinHandle<()>> {
        vec![tokio::spawn(terminal::serve(hub.clone(), config.server.bind.clone()))]
    }

    /// 停止所有串口。
    pub fn shutdown(&self) {
        let entries = std::mem::take(&mut *self.ports.lock().unwrap());
        for entry in entries.into_values() {
            stop_entry(entry);
        }
    }
}

fn stop_entry(entry: PortEntry) {
    for task in &entry.tasks {
        task.abort();
    }
    entry.hub.stop();
}

/// 对外监听且没有设置 token 时给出提醒：局域网内任何人都可以读写串口。
fn warn_if_exposed(config: &Config) {
    let loopback = config
        .server
        .bind
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    if !loopback && config.server.token.is_none() {
        tracing::warn!(
            bind = %config.server.bind,
            "服务对局域网开放且未设置 token，任何能访问本机的设备都可以读写串口"
        );
    }
}
