use std::path::PathBuf;

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
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    if args.list {
        print_devices();
        return Ok(());
    }

    let config_path = args.config.unwrap_or_else(Config::default_path);
    let config = Config::load(&config_path)?;
    if config.ports.is_empty() {
        tracing::warn!(
            "配置文件 {} 中没有串口，请参考 docs/DESIGN.md 添加 [[port]]",
            config_path.display()
        );
        print_devices();
    }
    run_headless(config, config_path)
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
