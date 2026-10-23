// release 构建不弹控制台窗口；--headless / --list 时再挂到父进程的控制台上输出。
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
#[command(version, about = "把本机串口通过局域网共享给终端和 AI")]
struct Args {
    /// 配置文件路径，默认 %APPDATA%\uarttool\config.toml
    #[arg(long, short)]
    config: Option<PathBuf>,
    /// 不启动界面，只运行后台服务（日志输出到控制台）
    #[arg(long)]
    headless: bool,
    /// 列出本机串口及 USB 序列号后退出
    #[arg(long)]
    list: bool,
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
    let gui_mode = !args.list && !args.headless;
    let screenshot = cut < argv.len();
    if gui_mode && !screenshot && windui::claim_instance(gui::APP_ID) == windui::InstanceRole::Handoff {
        return Ok(());
    }
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);

    if args.list || args.headless {
        attach_console();
        init_console_log();
    } else {
        init_file_log(&config_path);
    }

    if args.list {
        print_devices();
        return Ok(());
    }

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(e) if !args.headless => {
            // GUI 模式下配置有误也要能启动，用户可以在界面里打开配置文件修改
            tracing::error!("{e:#}，使用空配置启动");
            Config::default()
        }
        Err(e) => return Err(e),
    };
    if args.headless {
        if config.ports.is_empty() {
            tracing::warn!(
                "配置文件 {} 中没有串口，请参考 docs/DESIGN.md 添加 [[port]]",
                config_path.display()
            );
            print_devices();
        }
        run_headless(config, config_path)
    } else {
        run_gui(config, config_path, !args.autostart)
    }
}

fn run_headless(config: Config, config_path: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let service = Service::start(config, &config_path, |_| {})?;
        tracing::info!("服务已启动，按 Ctrl+C 退出");
        tokio::signal::ctrl_c().await?;
        service.shutdown();
        Ok(())
    })
}

fn run_gui(config: Config, config_path: PathBuf, open_window: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let service = {
        let _guard = runtime.enter();
        Service::start(config, &config_path, |_| {})?
    };
    gui::run(service.clone(), runtime.handle().clone(), open_window);
    service.shutdown();
    Ok(())
}

fn init_console_log() {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .init();
}

/// GUI 模式没有控制台，日志写到配置文件旁边的 uarttool.log。
fn init_file_log(config_path: &Path) {
    let path = config_path.with_file_name("uarttool.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
}

/// GUI 子系统的程序默认没有控制台；从命令行启动时挂到父进程的控制台上。
///
/// 输出被重定向（管道、文件）时标准句柄已经可用，不需要处理；直接在终端里运行时标准句柄为空，
/// 挂上控制台后要自己打开 `CONOUT$` 设为标准输出和标准错误（Rust 每次写入都会重新读取标准句柄）。
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::IntoRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
            STD_OUTPUT_HANDLE,
        };

        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                continue;
            }
            if let Ok(console) = OpenOptions::new().read(true).write(true).open("CONOUT$") {
                SetStdHandle(id, console.into_raw_handle());
            }
        }
    }
}

fn print_devices() {
    println!("{:<8} {:<10} {:<20} 描述", "COM", "VID:PID", "序列号");
    for d in uart_core::devices::list_devices() {
        let vid_pid = match (d.vid, d.pid) {
            (Some(v), Some(p)) => format!("{v:04X}:{p:04X}"),
            _ => "-".into(),
        };
        println!(
            "{:<8} {:<10} {:<20} {}",
            d.com,
            vid_pid,
            d.serial_number.as_deref().unwrap_or("-"),
            d.description.as_deref().unwrap_or("")
        );
    }
}
