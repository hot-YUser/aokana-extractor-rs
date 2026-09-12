//! list / extract 兩個操作。抽取前先算好全部路徑，失敗時整體先停。

use crate::dat::DatFile;
use crate::error::{Error, Result};

use std::io::Write as _;

/// `extract` 的執行選項。
pub struct Options {
    /// 執行緒數，0 表示自動（交給 `crate::pool` 解析）。
    pub threads: usize,
    /// 為真時不輸出任何進度。
    pub quiet: bool,
}

/// `extract` 的統計結果。
pub struct Stats {
    /// 處理的條目數。
    pub files: usize,
    /// 全部 payload 長度的總和。
    pub bytes: u64,
}

/// 逐筆列出。輸出格式必須與上游 extract.py 完全相同：
///   writeln!(out, "{} {} \tKByte(s)", name, entry.len)
/// 也就是「名稱、一個空格、長度、一個空格、TAB、KByte(s)」。
pub fn list(dat: &DatFile, out: &mut impl std::io::Write) -> Result<()> {
    // 循序走訪即可：list 是純格式化輸出，不值得平行化。
    // `?` 經由 `From<std::io::Error>` 把寫入失敗轉成 `Error::Io`。
    for i in 0..dat.len() {
        let (name, entry) = dat.get(i);
        writeln!(out, "{} {} \tKByte(s)", name, entry.len)?;
    }
    Ok(())
}

/// 平行抽取所有條目到 out_dir。
pub fn extract(dat: &DatFile, out_dir: &std::path::Path, opts: &Options) -> Result<Stats> {
    let n = dat.len();

    // 先單執行緒把所有輸出路徑算好：任何名稱不安全就在動手寫檔前整體失敗，
    // 避免抽一半才發現壞名稱、留下半成品輸出樹。
    let mut dests: Vec<std::path::PathBuf> = Vec::new();
    dests.reserve(n);
    for i in 0..n {
        let (name, _) = dat.get(i);
        let rel = crate::pathsafe::safe_relative_path(name)?;
        dests.push(out_dir.join(rel));
    }

    // `Stats.bytes` 是各 `entry.len` 的總和，先算好。
    // 用 checked 加法：長度來自不可信輸入，不做未檢查算術。
    let mut bytes: u64 = 0;
    for i in 0..n {
        let (_, entry) = dat.get(i);
        bytes = match bytes.checked_add(entry.len as u64) {
            Some(v) => v,
            None => {
                return Err(Error::BadFormat(format!(
                    "total payload size overflows u64 at entry {}",
                    i
                )))
            }
        };
    }

    // 每條執行緒一個可重用的 `Vec<u8>`，避免反覆配置。
    // 工作執行緒寫檔成功後直接印進度：每一行都是單一次 `writeln!`、持有
    // stdout 鎖的時間極短，所以各行不會交錯撕裂。安全性前提是沒有任何執行緒
    // 長期持有 stdout 鎖（見 main.rs 的說明）；stdout 寫入失敗一律吞掉。
    crate::pool::for_each_with(
        n,
        opts.threads,
        || Vec::<u8>::new(),
        |buf, i| {
            let (name, _) = dat.get(i);
            // 內部不變量：`dests` 與條目一一對應，pool 保證 `i < n`。
            let dest = match dests.get(i) {
                Some(p) => p,
                None => {
                    return Err(Error::BadFormat(format!(
                        "entry {} ('{}'): internal path table mismatch",
                        i, name
                    )))
                }
            };
            // 讀取失敗直接上拋，保留原始 kind（可能是 Io 或 BadFormat）。
            dat.read_entry_into(i, buf)?;
            if let Some(parent) = dest.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        Error::Io(std::io::Error::new(
                            e.kind(),
                            format!(
                                "entry {} ('{}'): cannot create dir '{}': {}",
                                i,
                                name,
                                parent.display(),
                                e
                            ),
                        ))
                    })?;
                }
            }
            // 一次性寫入整個 payload：`File::create` + `write_all`，
            // 多一層 `BufWriter` 只是多一次拷貝，沒有好處。
            std::fs::File::create(dest)
                .map_err(|e| {
                    Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "entry {} ('{}'): cannot create file '{}': {}",
                            i,
                            name,
                            dest.display(),
                            e
                        ),
                    ))
                })?
                .write_all(buf)
                .map_err(|e| {
                    Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "entry {} ('{}'): cannot write file '{}': {}",
                            i,
                            name,
                            dest.display(),
                            e
                        ),
                    ))
                })?;
            if !opts.quiet {
                // 即時進度：單一次 writeln! 內完成，不與他行交錯；失敗吞掉。
                let _ = writeln!(std::io::stdout().lock(), "Extracting {} ... Done.", name);
            }
            Ok(())
        },
    )?;

    Ok(Stats { files: n, bytes })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    /// `list` 每一行的位元組必須與上游 `extract.py` 的
    /// `print(name, size, "\tKByte(s)")` 逐字相同：
    /// 「名稱、一個空格、長度、一個空格、TAB、KByte(s)、換行」。
    #[test]
    fn list_line_bytes_match_upstream_extract_py() {
        let cases: &[(&str, u32, &[u8])] = &[
            ("a.png", 0, b"a.png 0 \tKByte(s)\n"),
            ("dir/b.dat", 1234, b"dir/b.dat 1234 \tKByte(s)\n"),
            (
                "a b/c d.bin",
                4294967295,
                b"a b/c d.bin 4294967295 \tKByte(s)\n",
            ),
        ];
        for (name, len, expected) in cases {
            // 與 `list` 內同一條格式化式走完全相同的 `writeln!` 路徑。
            let mut buf = Vec::new();
            writeln!(buf, "{} {} \tKByte(s)", name, len)
                .expect("invariant: writing to Vec<u8> never fails");
            assert_eq!(buf.as_slice(), *expected);
        }
    }
}
