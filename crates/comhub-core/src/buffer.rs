//! 日志行与内存环形缓冲。

use std::collections::VecDeque;
use std::fmt;

use chrono::{DateTime, Local};
use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// 设备发出的数据
    Rx,
    /// 写往设备的数据
    Tx,
    /// 服务自身的事件
    Sys,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Rx => "RX",
            Direction::Tx => "TX",
            Direction::Sys => "SYS",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogLine {
    /// 每个串口内从 1 开始递增的行号，用作增量读取的游标。
    pub seq: u64,
    pub time: DateTime<Local>,
    pub dir: Direction,
    pub text: String,
}

impl fmt::Display for LogLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}] {}",
            self.time.format("%Y-%m-%d %H:%M:%S%.3f"),
            self.dir.as_str(),
            self.text
        )
    }
}

#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    next_seq: u64,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity: capacity.max(1),
            next_seq: 1,
        }
    }

    pub fn push(&mut self, time: DateTime<Local>, dir: Direction, text: String) -> LogLine {
        let line = LogLine {
            seq: self.next_seq,
            time,
            dir,
            text,
        };
        self.next_seq += 1;
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
        line
    }

    /// 修改容量；变小时丢弃最旧的行。
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// 缓冲中最早一行的 seq；没有数据时为 `None`。
    pub fn first_seq(&self) -> Option<u64> {
        self.lines.front().map(|l| l.seq)
    }

    /// 最新一行的 seq；还没有数据时为 0。
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// 返回 seq 大于 `since` 的行，最多 `limit` 行（从旧到新）。
    ///
    /// `since` 比最新的 seq 还大，说明调用方拿的是服务重启前的游标，此时从头返回。
    pub fn since(&self, since: u64, limit: usize) -> Vec<LogLine> {
        let since = if since > self.last_seq() { 0 } else { since };
        let start = self.lines.partition_point(|l| l.seq <= since);
        self.lines.range(start..).take(limit).cloned().collect()
    }

    /// 最近 `n` 行（从旧到新）。
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let start = self.lines.len().saturating_sub(n);
        self.lines.range(start..).cloned().collect()
    }

    /// 最近 `limit` 条匹配的行（从旧到新）。
    pub fn grep(&self, re: &Regex, limit: usize) -> Vec<LogLine> {
        let mut found: Vec<LogLine> = self
            .lines
            .iter()
            .rev()
            .filter(|l| re.is_match(&l.text))
            .take(limit)
            .cloned()
            .collect();
        found.reverse();
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with(n: usize, capacity: usize) -> LogBuffer {
        let mut buf = LogBuffer::new(capacity);
        for i in 1..=n {
            buf.push(Local::now(), Direction::Rx, format!("line {i}"));
        }
        buf
    }

    fn seqs(lines: &[LogLine]) -> Vec<u64> {
        lines.iter().map(|l| l.seq).collect()
    }

    #[test]
    fn drops_oldest_when_full() {
        let buf = buffer_with(5, 3);
        assert_eq!(buf.last_seq(), 5);
        assert_eq!(seqs(&buf.tail(10)), vec![3, 4, 5]);
    }

    #[test]
    fn since_returns_newer_lines() {
        let buf = buffer_with(5, 10);
        assert_eq!(seqs(&buf.since(0, 100)), vec![1, 2, 3, 4, 5]);
        assert_eq!(seqs(&buf.since(3, 100)), vec![4, 5]);
        assert_eq!(seqs(&buf.since(1, 2)), vec![2, 3]);
        assert!(buf.since(5, 100).is_empty());
    }

    #[test]
    fn since_after_eviction_starts_from_oldest() {
        let buf = buffer_with(10, 3);
        assert_eq!(seqs(&buf.since(2, 100)), vec![8, 9, 10]);
    }

    #[test]
    fn since_from_future_cursor_restarts() {
        let buf = buffer_with(3, 10);
        assert_eq!(seqs(&buf.since(999, 100)), vec![1, 2, 3]);
    }

    #[test]
    fn shrinking_capacity_drops_oldest() {
        let mut buf = buffer_with(5, 10);
        buf.set_capacity(2);
        assert_eq!(buf.first_seq(), Some(4));
        buf.push(Local::now(), Direction::Rx, "x".into());
        assert_eq!(seqs(&buf.tail(10)), vec![5, 6]);
    }

    #[test]
    fn grep_returns_latest_matches_in_order() {
        let buf = buffer_with(12, 100);
        let re = Regex::new(r"line 1\d?$").unwrap();
        assert_eq!(seqs(&buf.grep(&re, 2)), vec![11, 12]);
        assert_eq!(seqs(&buf.grep(&re, 10)), vec![1, 10, 11, 12]);
    }

    #[test]
    fn display_format() {
        let mut buf = LogBuffer::new(1);
        let time = DateTime::parse_from_rfc3339("2026-10-23T14:57:01.123+08:00")
            .unwrap()
            .with_timezone(&Local);
        let line = buf.push(time, Direction::Tx, "reboot".into());
        let text = line.to_string();
        assert!(text.ends_with(" [TX] reboot"), "{text}");
        assert_eq!(text.len(), "2026-10-23 14:57:01.123 [TX] reboot".len());
    }
}
