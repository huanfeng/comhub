//! 不经过 HTTP、直接在本机执行的命令：枚举串口、无界面运行服务。

use std::path::PathBuf;

use anyhow::Result;
use uart_core::config::Config;
use uart_core::service::Service;

pub fn print_devices() {
    println!("{:<8} {:<10} {:<20} DESCRIPTION", "COM", "VID:PID", "SERIAL");
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

/// 在前台运行服务，日志输出到控制台，Ctrl+C 退出。
///
/// 与托盘程序共用同一份配置文件；两者同时运行时后启动的一方会因为串口和端口被占用而失败。
pub fn serve(config: Option<PathBuf>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = config.unwrap_or_else(Config::default_path);
    let config = Config::load(&config_path)?;
    tracing::info!("配置文件 {}", config_path.display());
    if config.ports.is_empty() {
        tracing::warn!("配置中没有串口，请参考 docs/DESIGN.md 添加 [[port]]；本机串口如下：");
        print_devices();
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let service = Service::start(config, &config_path, |_| {})?;
        tracing::info!("服务已启动，按 Ctrl+C 退出");
        tokio::signal::ctrl_c().await?;
        service.shutdown();
        Ok(())
    })
}
