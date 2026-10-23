//! 枚举本机串口，并按配置找到对应的设备。

use serde::Serialize;
use serialport::SerialPortType;

use crate::config::PortConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    /// 例如 `COM5`
    pub com: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub description: Option<String>,
}

pub fn list_devices() -> Vec<DeviceInfo> {
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            tracing::warn!("枚举串口失败: {e}");
            return Vec::new();
        }
    };
    let mut devices: Vec<DeviceInfo> = ports
        .into_iter()
        .map(|p| match p.port_type {
            SerialPortType::UsbPort(usb) => DeviceInfo {
                com: p.port_name,
                vid: Some(usb.vid),
                pid: Some(usb.pid),
                serial_number: usb.serial_number.filter(|sn| is_real_serial(sn)),
                description: usb.product,
            },
            SerialPortType::BluetoothPort => DeviceInfo {
                com: p.port_name,
                vid: None,
                pid: None,
                serial_number: None,
                description: Some("Bluetooth".into()),
            },
            _ => DeviceInfo {
                com: p.port_name,
                vid: None,
                pid: None,
                serial_number: None,
                description: None,
            },
        })
        .collect();
    devices.sort_by_key(|d| com_number(&d.com));
    devices
}

/// 在设备列表中查找配置对应的设备：配置了序列号就只按序列号匹配，否则按 COM 号匹配。
pub fn find_device<'a>(config: &PortConfig, devices: &'a [DeviceInfo]) -> Option<&'a DeviceInfo> {
    if let Some(sn) = &config.serial_number {
        return devices
            .iter()
            .find(|d| d.serial_number.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(sn)));
    }
    let com = config.com.as_deref()?;
    devices.iter().find(|d| d.com.eq_ignore_ascii_case(com))
}

/// 没有序列号的 USB 设备，Windows 会生成形如 `8&2D893146&0&4` 的实例 ID，
/// serialport 会从中截出 `8` 当作序列号。这种值随插入的 USB 口变化，不能用来识别设备。
fn is_real_serial(sn: &str) -> bool {
    sn.len() >= 4 && !sn.contains('&')
}

/// COM 号的数字部分，用于排序（COM10 排在 COM9 后面）。
fn com_number(com: &str) -> u32 {
    com.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Encoding, TerminalMode};

    fn device(com: &str, sn: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            com: com.into(),
            vid: None,
            pid: None,
            serial_number: sn.map(Into::into),
            description: None,
        }
    }

    fn config(sn: Option<&str>, com: Option<&str>) -> PortConfig {
        PortConfig {
            name: "x".into(),
            serial_number: sn.map(Into::into),
            com: com.map(Into::into),
            baud: 115_200,
            tcp_port: 7001,
            mode: TerminalMode::Telnet,
            encoding: Encoding::Utf8,
            enabled: true,
        }
    }

    #[test]
    fn serial_number_takes_priority_over_com() {
        let devices = [device("COM3", Some("AAA")), device("COM7", Some("BBB"))];
        let found = find_device(&config(Some("bbb"), Some("COM3")), &devices).unwrap();
        assert_eq!(found.com, "COM7");
        assert!(find_device(&config(Some("CCC"), Some("COM3")), &devices).is_none());
    }

    #[test]
    fn falls_back_to_com() {
        let devices = [device("COM3", None)];
        assert!(find_device(&config(None, Some("com3")), &devices).is_some());
        assert!(find_device(&config(None, Some("COM4")), &devices).is_none());
    }

    #[test]
    fn rejects_generated_instance_ids() {
        assert!(is_real_serial("A50285BIA"));
        assert!(!is_real_serial("8"));
        assert!(!is_real_serial("8&2D893146&0&4"));
    }

    #[test]
    fn com_number_sorting() {
        assert!(com_number("COM9") < com_number("COM10"));
    }
}
