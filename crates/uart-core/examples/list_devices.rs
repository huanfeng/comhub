//! 打印本机串口及其 USB 信息：cargo run -p uart-core --example list_devices
fn main() {
    for d in uart_core::devices::list_devices() {
        println!("{d:?}");
    }
}
