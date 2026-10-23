//! 开机自启：写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。

use anyhow::Result;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "uarttool";
/// 自启动时带上这个参数：只显示托盘，不弹主窗口。
pub const AUTOSTART_ARG: &str = "--autostart";

pub fn is_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|key| key.get_value::<String, _>(VALUE_NAME))
        .is_ok()
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value(VALUE_NAME, &format!("\"{}\" {AUTOSTART_ARG}", exe.display()))?;
    } else if key.get_value::<String, _>(VALUE_NAME).is_ok() {
        key.delete_value(VALUE_NAME)?;
    }
    Ok(())
}
