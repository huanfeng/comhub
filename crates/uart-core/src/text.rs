//! 字节流 → 日志行：行切分、ANSI 清洗、解码。

use std::time::{Duration, Instant};

use crate::config::Encoding;

/// 单行最长字节数，超过就强制切行。
pub const MAX_LINE_BYTES: usize = 4096;

/// 切分的是哪个方向的数据。两个方向的切行规则不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitMode {
    /// 设备输出：`\n` 切行，`\r` 丢弃，空闲 100ms 把未结束的内容写出。
    Rx,
    /// 写往设备：`\r` 或 `\n` 切行，处理退格；终端逐字符发送，所以空闲超时放宽到 1s。
    Tx,
}

impl SplitMode {
    fn idle(self) -> Duration {
        match self {
            SplitMode::Rx => Duration::from_millis(100),
            SplitMode::Tx => Duration::from_secs(1),
        }
    }
}

#[derive(Debug)]
pub struct LineSplitter {
    mode: SplitMode,
    buf: Vec<u8>,
    last_data: Option<Instant>,
    /// TX 模式下，上一个字节是否是 `\r`（用来吞掉 `\r\n` 里的 `\n`）。
    prev_cr: bool,
}

impl LineSplitter {
    pub fn new(mode: SplitMode) -> Self {
        Self {
            mode,
            buf: Vec::new(),
            last_data: None,
            prev_cr: false,
        }
    }

    /// 喂入一段数据，返回其中完整的行（不含换行符）。
    pub fn feed(&mut self, data: &[u8], now: Instant) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        for &b in data {
            match (self.mode, b) {
                (SplitMode::Rx, b'\r') => {}
                (SplitMode::Rx, b'\n') => lines.push(std::mem::take(&mut self.buf)),
                (SplitMode::Tx, b'\n') if self.prev_cr => {}
                (SplitMode::Tx, b'\r' | b'\n') => lines.push(std::mem::take(&mut self.buf)),
                (SplitMode::Tx, 0x08 | 0x7f) => {
                    pop_char(&mut self.buf);
                }
                _ => {
                    self.buf.push(b);
                    if self.buf.len() >= MAX_LINE_BYTES {
                        lines.push(std::mem::take(&mut self.buf));
                    }
                }
            }
            self.prev_cr = b == b'\r';
        }
        if !data.is_empty() {
            self.last_data = Some(now);
        }
        lines
    }

    /// 空闲超时后，把未结束的内容作为一行取出。
    pub fn poll_idle(&mut self, now: Instant) -> Option<Vec<u8>> {
        match self.deadline() {
            Some(deadline) if now >= deadline => self.take(),
            _ => None,
        }
    }

    /// 下一次需要调用 [`poll_idle`](Self::poll_idle) 的时间；没有待写出的内容时返回 `None`。
    pub fn deadline(&self) -> Option<Instant> {
        if self.buf.is_empty() {
            return None;
        }
        self.last_data.map(|t| t + self.mode.idle())
    }

    /// 立即取出未结束的内容（例如设备断开时）。
    pub fn take(&mut self) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

/// 退格：删掉最后一个字符（UTF-8 多字节字符整个删掉）。
fn pop_char(buf: &mut Vec<u8>) {
    while let Some(b) = buf.pop() {
        // UTF-8 后续字节是 10xxxxxx，继续删到首字节为止。
        if b & 0xC0 != 0x80 {
            break;
        }
    }
}

/// 把一行原始字节变成适合阅读的文本：去掉 ANSI 转义序列和控制字符，再按编码解码。
pub fn render_line(raw: &[u8], encoding: Encoding) -> String {
    let cleaned = strip_ansi(raw);
    match encoding {
        Encoding::Utf8 => decode_utf8_escaped(&cleaned),
        Encoding::Gbk => encoding_rs::GBK.decode_without_bom_handling(&cleaned).0.into_owned(),
    }
}

/// 去掉 ANSI 转义序列（CSI / OSC / 两字节序列）以及除 Tab 外的 C0 控制字符。
///
/// 转义序列全部由 ASCII 字节组成，而 UTF-8 和 GBK 的多字节字符里都不会出现 0x00–0x1F，
/// 所以可以在解码前按字节处理。
fn strip_ansi(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b == 0x1B {
            i += 1;
            match raw.get(i) {
                // CSI: ESC [ 参数... 结束字节(0x40–0x7E)
                Some(b'[') => {
                    i += 1;
                    while i < raw.len() && !(0x40..=0x7E).contains(&raw[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                // OSC: ESC ] ... 以 BEL 或 ESC \ 结束
                Some(b']') => {
                    i += 1;
                    while i < raw.len() {
                        if raw[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if raw[i] == 0x1B && raw.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 1,
                None => {}
            }
            continue;
        }
        if b >= 0x20 || b == b'\t' {
            out.push(b);
        }
        i += 1;
    }
    out
}

/// 按 UTF-8 解码，无法解码的字节表示为 `\xNN`。
fn decode_utf8_escaped(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(s: &mut LineSplitter, data: &[u8]) -> Vec<String> {
        s.feed(data, Instant::now())
            .into_iter()
            .map(|l| String::from_utf8(l).unwrap())
            .collect()
    }

    #[test]
    fn rx_splits_on_lf_and_drops_cr() {
        let mut s = LineSplitter::new(SplitMode::Rx);
        assert_eq!(feed_all(&mut s, b"hello\r\nwor"), vec!["hello"]);
        assert_eq!(feed_all(&mut s, b"ld\n\n"), vec!["world", ""]);
        assert!(s.take().is_none());
    }

    #[test]
    fn rx_flushes_partial_line_after_idle() {
        let mut s = LineSplitter::new(SplitMode::Rx);
        let t0 = Instant::now();
        assert!(s.feed(b"login: ", t0).is_empty());
        assert_eq!(s.deadline(), Some(t0 + Duration::from_millis(100)));
        assert!(s.poll_idle(t0 + Duration::from_millis(50)).is_none());
        assert_eq!(s.poll_idle(t0 + Duration::from_millis(100)).unwrap(), b"login: ");
        assert!(s.deadline().is_none());
    }

    #[test]
    fn splits_overlong_line() {
        let mut s = LineSplitter::new(SplitMode::Rx);
        let lines = s.feed(&vec![b'a'; MAX_LINE_BYTES + 10], Instant::now());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(s.take().unwrap().len(), 10);
    }

    #[test]
    fn tx_handles_cr_crlf_and_backspace() {
        let mut s = LineSplitter::new(SplitMode::Tx);
        // 终端逐字符输入，带一次退格
        let mut lines = Vec::new();
        for b in b"rebox\x7Fot\r" {
            lines.extend(feed_all(&mut s, &[*b]));
        }
        assert_eq!(lines, vec!["reboot"]);
        // \r\n 只算一次换行
        assert_eq!(feed_all(&mut s, b"ls\r\nps\n"), vec!["ls", "ps"]);
    }

    #[test]
    fn backspace_removes_whole_utf8_char() {
        let mut s = LineSplitter::new(SplitMode::Tx);
        assert_eq!(feed_all(&mut s, "a中\x08b\r".as_bytes()), vec!["ab"]);
    }

    #[test]
    fn render_strips_ansi_and_controls() {
        let raw = b"\x1b[0;32mI (312) wifi: ok\x1b[0m\x07\tend\x1b]0;title\x07";
        assert_eq!(render_line(raw, Encoding::Utf8), "I (312) wifi: ok\tend");
    }

    #[test]
    fn render_escapes_invalid_utf8() {
        assert_eq!(render_line(b"ok\xFF\xFEz", Encoding::Utf8), "ok\\xFF\\xFEz");
        assert_eq!(render_line("温度".as_bytes(), Encoding::Utf8), "温度");
    }

    #[test]
    fn render_decodes_gbk() {
        // "温度" 的 GBK 编码
        assert_eq!(render_line(&[0xCE, 0xC2, 0xB6, 0xC8], Encoding::Gbk), "温度");
    }
}
