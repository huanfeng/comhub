//! 配置文件（TOML）的读写与校验。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(rename = "port")]
    pub ports: Vec<PortConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// HTTP 与 Terminal 服务共用的监听地址。
    pub bind: String,
    pub http_port: u16,
    /// 为空时使用配置文件所在目录下的 `logs/`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// 每个串口在内存中保留的行数。
    pub buffer_lines: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".into(),
            http_port: 7000,
            log_dir: None,
            token: None,
            buffer_lines: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub com: Option<String>,
    #[serde(default = "default_baud")]
    pub baud: u32,
    pub tcp_port: u16,
    #[serde(default)]
    pub mode: TerminalMode,
    #[serde(default)]
    pub encoding: Encoding,
    /// 为 false 时不打开该串口（保留配置）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_baud() -> u32 {
    115_200
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalMode {
    #[default]
    Telnet,
    Raw,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    #[default]
    #[serde(rename = "utf-8", alias = "utf8")]
    Utf8,
    #[serde(rename = "gbk")]
    Gbk,
}

impl Config {
    /// 默认配置文件路径：`%APPDATA%\comhub\config.toml`。
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("comhub")
            .join("config.toml")
    }

    /// 读取配置；文件不存在时返回默认配置。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        let mut tcp_ports = HashSet::from([self.server.http_port]);
        for port in &self.ports {
            if !is_valid_name(&port.name) {
                bail!("串口别名 `{}` 不合法：只能使用字母、数字、- 和 _", port.name);
            }
            if !names.insert(port.name.to_ascii_lowercase()) {
                bail!("串口别名 `{}` 重复", port.name);
            }
            if port.serial_number.is_none() && port.com.is_none() {
                bail!("串口 `{}` 需要配置 serial_number 或 com", port.name);
            }
            if !tcp_ports.insert(port.tcp_port) {
                bail!("串口 `{}` 的 tcp_port {} 与其它端口冲突", port.name, port.tcp_port);
            }
        }
        Ok(())
    }

    /// 日志根目录：优先使用配置项，否则是配置文件旁边的 `logs/`。
    pub fn log_dir(&self, config_path: &Path) -> PathBuf {
        match &self.server.log_dir {
            Some(dir) => dir.clone(),
            None => config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("logs"),
        }
    }
}

pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let config: Config = toml::from_str(
            r#"
            [[port]]
            name = "esp32"
            serial_number = "A50285BI"
            tcp_port = 7001
            "#,
        )
        .unwrap();
        assert_eq!(config.server.http_port, 7000);
        let port = &config.ports[0];
        assert_eq!(port.baud, 115_200);
        assert_eq!(port.mode, TerminalMode::Telnet);
        assert_eq!(port.encoding, Encoding::Utf8);
        assert!(port.enabled);
        config.validate().unwrap();
    }

    #[test]
    fn parses_encoding_and_mode() {
        let config: Config = toml::from_str(
            r#"
            [[port]]
            name = "stm32"
            com = "COM3"
            tcp_port = 7002
            mode = "raw"
            encoding = "gbk"
            "#,
        )
        .unwrap();
        assert_eq!(config.ports[0].mode, TerminalMode::Raw);
        assert_eq!(config.ports[0].encoding, Encoding::Gbk);
    }

    #[test]
    fn rejects_duplicate_names_and_ports() {
        let base = PortConfig {
            name: "a".into(),
            serial_number: None,
            com: Some("COM1".into()),
            baud: 115_200,
            tcp_port: 7001,
            mode: TerminalMode::Telnet,
            encoding: Encoding::Utf8,
            enabled: true,
        };
        let mut config = Config {
            server: ServerConfig::default(),
            ports: vec![base.clone(), PortConfig { tcp_port: 7002, ..base.clone() }],
        };
        assert!(config.validate().is_err(), "重复别名应报错");

        config.ports[1].name = "b".into();
        config.ports[1].tcp_port = 7001;
        assert!(config.validate().is_err(), "重复端口应报错");

        config.ports[1].tcp_port = 7000;
        assert!(config.validate().is_err(), "与 HTTP 端口冲突应报错");

        config.ports[1].tcp_port = 7002;
        config.validate().unwrap();
    }

    #[test]
    fn rejects_port_without_match_rule() {
        let config: Config = toml::from_str(
            r#"
            [[port]]
            name = "x"
            tcp_port = 7001
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        let mut config = Config::default();
        config.ports.push(PortConfig {
            name: "esp32".into(),
            serial_number: Some("SN1".into()),
            com: None,
            baud: 921_600,
            tcp_port: 7001,
            mode: TerminalMode::Raw,
            encoding: Encoding::Gbk,
            enabled: false,
        });
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert_eq!(config.log_dir(&path), dir.path().join("sub").join("logs"));
    }
}
