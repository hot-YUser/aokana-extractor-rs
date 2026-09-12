//! 平台差異封裝：位置讀取是平行抽取的基礎，必須繞開檔案游標。

use crate::error::{Error, Result};

/// 檔案長度。
pub fn file_len(f: &std::fs::File) -> Result<u64> {
    // 為什麼用 metadata 而不是 seek 到尾端：不碰游標，多執行緒共用同一個 File 才安全。
    Ok(f.metadata().map(|m| m.len()).map_err(Error::Io)?)
}

/// 從某個平台相關的位置讀取一批位元組，絕不改變檔案游標。
///
/// 為什麼拆成兩個 `#[cfg]` 私有函式：Unix 的 `read_at` 與 Windows 的 `seek_read`
/// 是不同 trait 的同名概念，呼叫端不該看到差異。
#[cfg(unix)]
fn read_at_once(f: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, offset).map_err(Error::Io)
}

/// 見 unix 版註解。
#[cfg(windows)]
fn read_at_once(f: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, offset).map_err(Error::Io)
}

/// 後備平台：沒有原生位置讀取 API 時明確報錯，而不是偷偷用 seek 模擬
///（seek 模擬會把平行讀取序列化，違反本模組的核心不變量）。
#[cfg(not(any(unix, windows)))]
fn read_at_once(
    _f: &std::fs::File,
    _buf: &mut [u8],
    _offset: u64,
) -> Result<usize> {
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "positional read is not supported on this platform",
    )))
}

/// 從絕對位移讀滿 `buf`，不改變檔案游標。這是平行讀取的核心：
/// 多執行緒共用同一個 File 也不需要任何鎖。
pub fn read_exact_at(f: &std::fs::File, buf: &mut [u8], offset: u64) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let wanted = buf.len();
    let mut done: usize = 0;
    // 為什麼用迴圈補滿：原生 API 允許短讀（例如接近 EOF），一次呼叫不保證填滿。
    while done < wanted {
        let off = offset.checked_add(done as u64).ok_or_else(|| {
            Error::BadFormat(format!(
                "offset overflow: base {offset} + done {done} (wanted {wanted} bytes)"
            ))
        })?;
        // 用 checked 切片而非直接索引：呼叫端傳入的長度不可信，越界時回傳錯誤而非 panic。
        let rest = buf.get_mut(done..).ok_or_else(|| {
            Error::BadFormat(format!(
                "offset {offset}: cursor {done} out of bounds (wanted {wanted} bytes)"
            ))
        })?;
        let n = read_at_once(f, rest, off)?;
        if n == 0 {
            // 讀到 0 位元組表示 EOF：要求長度沒被滿足，是損壞或截斷的輸入。
            return Err(Error::BadFormat(format!(
                "unexpected EOF at offset {offset}: wanted {wanted} bytes, got {done} bytes"
            )));
        }
        done = done.checked_add(n).ok_or_else(|| {
            Error::BadFormat(format!(
                "offset {offset}: byte count overflow (done {done} + got {n}, wanted {wanted} bytes)"
            ))
        })?;
        // 原生 API 的契約保證 n <= rest.len()，但多做一層檢查，避免不可信的後端造成無窮迴圈。
        if done > wanted {
            return Err(Error::BadFormat(format!(
                "offset {offset}: read {done} bytes, exceeding wanted {wanted} bytes"
            )));
        }
    }
    Ok(())
}

/// 邏輯 CPU 數，至少為 1。以 std::thread::available_parallelism 實作。
pub fn available_parallelism() -> usize {
    // 為什麼失敗時回傳 1 而不是報錯：呼叫端（pool）只拿它當執行緒數 hint，
    // 取不到時用單執行緒退化即可，不值得讓整個流程失敗。
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
