//! 按天滚动的日志文件：`<root>/<name>/<name>-YYYY-MM-DD.log`。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::buffer::LogLine;

pub struct LogFile {
    dir: PathBuf,
    name: String,
    current: Option<(NaiveDate, BufWriter<File>)>,
    failing: bool,
}

impl LogFile {
    pub fn new(root: &Path, name: &str) -> Self {
        Self {
            dir: root.join(name),
            name: name.to_string(),
            current: None,
            failing: false,
        }
    }

    pub fn path_for(&self, date: NaiveDate) -> PathBuf {
        self.dir
            .join(format!("{}-{}.log", self.name, date.format("%Y-%m-%d")))
    }

    /// 写入一行。写失败只记录警告，不影响串口数据的分发。
    ///
    /// 持续失败（例如磁盘满）时只在开始失败和恢复时各记一次，不逐行刷日志。
    pub fn write(&mut self, line: &LogLine) {
        match self.try_write(line) {
            Ok(()) if self.failing => {
                self.failing = false;
                tracing::info!(port = %self.name, "日志文件恢复写入");
            }
            Ok(()) => {}
            Err(e) => {
                if !self.failing {
                    tracing::warn!(port = %self.name, "写日志文件失败: {e}");
                    self.failing = true;
                }
                self.current = None;
            }
        }
    }

    fn try_write(&mut self, line: &LogLine) -> std::io::Result<()> {
        let date = line.time.date_naive();
        if self.current.as_ref().map(|(d, _)| *d) != Some(date) {
            self.flush();
            std::fs::create_dir_all(&self.dir)?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path_for(date))?;
            self.current = Some((date, BufWriter::new(file)));
        }
        let (_, writer) = self.current.as_mut().expect("刚刚打开过");
        writeln!(writer, "{line}")
    }

    /// 把缓冲写到磁盘。每批数据处理完调用一次，让外部读取到的日志足够新。
    pub fn flush(&mut self) {
        if let Some((_, writer)) = self.current.as_mut() {
            if let Err(e) = writer.flush() {
                tracing::warn!(port = %self.name, "刷新日志文件失败: {e}");
            }
        }
    }
}

impl Drop for LogFile {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Direction, LogBuffer};
    use chrono::{DateTime, Local};

    fn at(rfc3339: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn writes_and_rotates_by_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = LogFile::new(dir.path(), "esp32");
        let mut buf = LogBuffer::new(10);

        let day1 = at("2026-10-23T12:00:00+00:00");
        let day2 = at("2026-10-25T12:00:00+00:00");
        file.write(&buf.push(day1, Direction::Rx, "a".into()));
        file.write(&buf.push(day1, Direction::Tx, "b".into()));
        file.write(&buf.push(day2, Direction::Sys, "c".into()));
        file.flush();

        let first = std::fs::read_to_string(file.path_for(day1.date_naive())).unwrap();
        let second = std::fs::read_to_string(file.path_for(day2.date_naive())).unwrap();
        assert_eq!(first.lines().count(), 2);
        assert!(first.lines().nth(1).unwrap().ends_with("[TX] b"));
        assert!(second.trim_end().ends_with("[SYS] c"));
    }
}
