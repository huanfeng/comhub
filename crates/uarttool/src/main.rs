// release 构建不弹控制台窗口。命令行功能（列出串口、无界面运行服务）在 uartctl 里。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod gui;
mod icon;

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use clap::Parser;
use uart_core::config::Config;
use uart_core::service::Service;

#[derive(Parser)]
#[command(version, about = "把本机串口通过局域网共享给终端和 AI（托盘程序）")]
struct Args {
    /// 配置文件路径，默认 %APPDATA%\uarttool\config.toml
    #[arg(long, short)]
    config: Option<PathBuf>,
    /// 开机自启时使用：只显示托盘图标，不打开主窗口
    #[arg(long)]
    autostart: bool,
}

fn main() -> Result<()> {
    // windui 的离屏截图参数（`--screenshot <path>` 及其后的参数）交给 windui 自己解析，
    // 约定放在命令行末尾，前面的部分照常由 clap 解析。
    let argv: Vec<String> = std::env::args().collect();
    let cut = argv.iter().position(|a| a == "--screenshot").unwrap_or(argv.len());
    let args = Args::parse_from(&argv[..cut]);

    // 单实例：已有实例在运行时把参数转交给它（它会打开主窗口）后直接退出。
    // 必须在启动服务之前判断，否则第二个实例会去抢占串口、往同一份日志里写错误。
    let screenshot = cut < argv.len();
    if !screenshot && windui::claim_instance(gui::APP_ID) == windui::InstanceRole::Handoff {
        return Ok(());
    }

    let config_path = args.config.unwrap_or_else(Config::default_path);
    init_file_log(&config_path);

    // 配置有误也要能启动，用户可以在界面里打开配置文件修改；
    // 界面会提示错误，并在修正之前禁止保存，避免用空配置覆盖原文件。
    let (config, load_error) = match Config::load(&config_path) {
        Ok(config) => (config, None),
        Err(e) => {
            tracing::error!("{e:#}，使用空配置启动");
            (Config::default(), Some(format!("{e:#}")))
        }
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let service = {
        let _guard = runtime.enter();
        Service::start(config, &config_path, |_| {})?
    };
    gui::run(service.clone(), runtime.handle().clone(), load_error, !args.autostart);
    service.shutdown();
    Ok(())
}

/// GUI 程序没有控制台，日志写到配置文件旁边的 uarttool.log。
fn init_file_log(config_path: &Path) {
    let path = config_path.with_file_name("uarttool.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
}
