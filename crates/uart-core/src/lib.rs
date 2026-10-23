//! uarttool 的服务核心：独占物理串口，把数据分发给终端、HTTP 客户端和日志文件。

pub mod buffer;
pub mod config;
pub mod logfile;
pub mod text;
