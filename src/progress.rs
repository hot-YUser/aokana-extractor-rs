//! 終端進度條：單行更新、含預估剩餘時間。

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 兩次渲染之間的最小間隔。太頻繁會拖慢抽取，太疏會看起來卡住。
const TICK_MS: u64 = 100;
/// 進度條本體的寬度（字元數）。
const BAR_WIDTH: usize = 28;
/// `last_ms` 的初始值，表示「還沒渲染過」。
const NEVER: u64 = u64::MAX;

/// 跨執行緒共享的位元組進度。
///
/// 工作執行緒只做一次原子累加，渲染由「搶到渲染權」的那條執行緒負責，
/// 因此不需要額外的渲染執行緒、不需要 `Arc`、也不需要鎖。
pub struct Progress {
    total: u64,
    done: AtomicU64,
    last_ms: AtomicU64,
    start: Instant,
    enabled: bool,
}

impl Progress {
    /// `quiet`、總量為 0、或 stdout 不是終端機時停用（停用後所有方法仍是安全的無操作）。
    pub fn new(total: u64, quiet: bool) -> Self {
        Self {
            total,
            done: AtomicU64::new(0),
            last_ms: AtomicU64::new(NEVER),
            start: Instant::now(),
            // 重導向後 `\r` 會污染輸出，所以只在真正的終端機上啟用。
            enabled: !quiet && total > 0 && std::io::stdout().is_terminal(),
        }
    }

    /// 工作執行緒回報完成 `bytes` 個位元組。
    /// 以 `last_ms.swap(now)` 做速率限制：只有 swap 回傳的舊值距現在 >= TICK_MS
    /// 的那一條執行緒會真的渲染（swap 是原子交換，所以同批只會有一條通過）。
    pub fn add(&self, bytes: u64) {
        if !self.enabled {
            return;
        }
        self.done.fetch_add(bytes, Ordering::Relaxed);
        let now = self.start.elapsed().as_millis() as u64;
        let prev = self.last_ms.swap(now, Ordering::Relaxed);
        // 首次呼叫（prev == NEVER）一定要畫一次，否則小檔案跑完都沒畫面。
        if prev == NEVER || now.saturating_sub(prev) >= TICK_MS {
            self.render();
        }
    }

    /// 主執行緒在全部工作結束後呼叫：渲染 100% 並換行。
    pub fn finish(&self) {
        if !self.enabled {
            return;
        }
        let line = format_line(self.total, self.total, self.start.elapsed());
        let mut out = std::io::stdout();
        // 管線中斷不該讓解包失敗，所以寫入失敗一律吞掉。
        let _ = writeln!(out, "\r{line}");
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    fn render(&self) {
        let done = self.done.load(Ordering::Relaxed).min(self.total);
        let line = format_line(self.total, done, self.start.elapsed());
        let mut out = std::io::stdout();
        let _ = write!(out, "\r{line}");
        let _ = out.flush();
    }
}

/// 組出一整行進度文字（不含 `\r`、`\n`，由呼叫端決定覆蓋還是換行）。
fn format_line(total: u64, done: u64, elapsed: Duration) -> String {
    // 用 u128 先乘後除，位元組數大時才不會溢出。
    let pct = if total == 0 {
        0
    } else {
        (done as u128 * 100 / total as u128) as u64
    };
    let filled = if total == 0 {
        0
    } else {
        (done as u128 * BAR_WIDTH as u128 / total as u128) as usize
    }
    .min(BAR_WIDTH);
    let eta = if done == 0 {
        // 還沒進展時除法無意義，直接顯示未知。
        String::from("--:--")
    } else {
        let remain = elapsed.as_secs_f64() * (total - done) as f64 / done as f64;
        fmt_eta(remain as u64)
    };
    let mut bar = String::with_capacity(BAR_WIDTH);
    for _ in 0..filled {
        bar.push('#');
    }
    for _ in filled..BAR_WIDTH {
        bar.push('-');
    }
    format!(
        "[{bar}] {pct}%  {}/{}  ETA {eta}",
        fmt_bytes(done),
        fmt_bytes(total),
    )
}

/// 以 1024 為底：`>= 1 GiB` 用 `G`、`>= 1 MiB` 用 `M`、`>= 1 KiB` 用 `K`，
/// `K` 以下直接用 `B`（數量小，小數無意義）。
fn fmt_bytes(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= G {
        trim1(b / G, 'G')
    } else if b >= M {
        trim1(b / M, 'M')
    } else if b >= K {
        trim1(b / K, 'K')
    } else {
        format!("{bytes}B")
    }
}

/// 取一位小數，但整數時去掉 `.0`（`845.0M` 讀起來不如 `845M` 乾淨）。
fn trim1(v: f64, suffix: char) -> String {
    let s = format!("{v:.1}");
    let t = s.strip_suffix(".0").unwrap_or(&s);
    format!("{t}{suffix}")
}

/// 小於一小時用 `mm:ss`，否則 `h:mm:ss`（小時不補零，分鐘秒數補零）。
fn fmt_eta(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_below_k_show_plain_b() {
        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(1), "1B");
        assert_eq!(fmt_bytes(1023), "1023B");
    }

    #[test]
    fn bytes_k_m_g_keep_one_decimal() {
        assert_eq!(fmt_bytes(12 * 1024), "12K");
        assert_eq!(fmt_bytes(1536), "1.5K");
        assert_eq!(fmt_bytes(845 * 1024 * 1024), "845M");
        assert_eq!(fmt_bytes((2.8 * 1024.0 * 1024.0 * 1024.0) as u64), "2.8G");
    }

    #[test]
    fn eta_under_an_hour_is_mm_ss() {
        assert_eq!(fmt_eta(0), "00:00");
        assert_eq!(fmt_eta(61), "01:01");
        assert_eq!(fmt_eta(3599), "59:59");
    }

    #[test]
    fn eta_over_an_hour_is_h_mm_ss() {
        assert_eq!(fmt_eta(3600), "1:00:00");
        assert_eq!(fmt_eta(3661), "1:01:01");
    }

    #[test]
    fn full_bar_renders_at_full_percent() {
        let line = format_line(100, 100, Duration::from_secs(5));
        assert!(line.starts_with("[############################] 100% "));
        assert!(line.ends_with("ETA 00:00"));
    }

    #[test]
    fn zero_done_shows_unknown_eta() {
        let line = format_line(100, 0, Duration::from_secs(5));
        assert!(line.contains("--:--"));
    }
}
