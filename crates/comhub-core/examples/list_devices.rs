//! 打印本机串口及其 USB 信息：cargo run -p comhub-core --example list_devices
fn main() {
    for d in comhub_core::devices::list_devices() {
        println!("{d:?}");
    }
}
