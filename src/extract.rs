//! 多 .dat 解包：收集、攤平、單一進度條平行寫出。
//!
//! `run` 先把輸入（單一 .dat 或資料夾）收集成已排序的清單，
//! 再把所有 entry 攤平成一條工作清單、共用同一個 `Progress` 跑完。

use crate::dat::DatFile;
use crate::error::{Error, Result};
use crate::progress::Progress;

use std::io::Write as _;
use std::time::Instant;

/// `run` 的執行選項。
pub struct Options {
    /// 執行緒數，0 表示自動（交給 `crate::pool` 解析）。
    pub threads: usize,
    /// 為真時不顯示進度。
    pub quiet: bool,
}

/// `run` 的統計結果（摘要由 `cli` 負責印，這裡只回資料）。
pub struct Stats {
    /// 處理的 .dat 個數。
    pub dats: usize,
    /// 寫出的檔案（entry）總數。
    pub files: usize,
    /// 全部 payload 長度的總和。
    pub bytes: u64,
    /// `run` 開始到結束的耗時。
    pub elapsed: std::time::Duration,
    /// 掃到但開不起來、因此被略過的 .dat 數量。
    pub skipped: usize,
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

/// 解包 `input`（單一 .dat 或資料夾）底下的每一個 .dat。
/// `out` 為 `Some` 時當作輸出根目錄並鏡射來源結構；`None` 時解到各 .dat 自己所在目錄。
pub fn run(
    input: &std::path::Path,
    out: Option<&std::path::Path>,
    opts: &Options,
) -> Result<Stats> {
    let start = Instant::now();
    // 先收集完整清單才動手：解包會在原地建立新資料夾，
    // 若邊走邊解會走進剛建好的輸出資料夾。
    let dats = collect_dats(input)?;
    // 資料夾 + `-o` 要鏡射相對結構，單一檔案 + `-o` 則直接掛 stem；
    // 兩者的輸出算法不同，先記住輸入是哪一種。
    let input_is_file = matches!(std::fs::metadata(input), Ok(m) if m.is_file());

    // 逐一算好輸出資料夾並開啟：開檔失敗一樣在寫檔前回報，不留半成品。
    // 資料夾裡可能混著不是封裝格式的 .dat（例如解出來的 system.dat）：
    // 開得起來的才納入工作清單，開不起來的記下並繼續，不讓單一壞檔中止整批。
    let mut jobs: Vec<DatFile> = Vec::with_capacity(dats.len());
    let mut bases: Vec<std::path::PathBuf> = Vec::with_capacity(dats.len());
    let mut ok_dats: Vec<std::path::PathBuf> = Vec::with_capacity(dats.len());
    let mut skipped: Vec<(std::path::PathBuf, Error)> = Vec::new();
    for dat_path in dats.iter() {
        let base = match out {
            None => default_out_dir(dat_path)?,
            Some(root) if input_is_file => single_out_dir(dat_path, root)?,
            Some(root) => mirror_out_dir(input, dat_path, root)?,
        };
        match DatFile::open(dat_path) {
            Ok(job) => {
                jobs.push(job);
                bases.push(base);
                ok_dats.push(dat_path.clone());
            }
            Err(e) => {
                skipped.push((dat_path.clone(), e));
            }
        }
    }
    if jobs.is_empty() {
        // 全部都開不起來：回第一個開檔錯誤，讓使用者看到真正的原因。
        match skipped.into_iter().next() {
            Some((_, e)) => return Err(e),
            // 走不到：dats 非空時每筆不是進 jobs 就是進 skipped。
            // 仍回錯誤不 panic；訊息與掃描階段找不到 .dat 時同格式。
            None => {
                return Err(Error::BadFormat(format!(
                    "no .dat found under '{}'",
                    input.display()
                )))
            }
        }
    }
    // 開檔階段全部發生在 Progress 開始之前：在這裡把略過印完，
    // 進度條才不會被中途插進的文字弄髒（與進度條同一條 stdout）。
    if !opts.quiet {
        let mut so = std::io::stdout();
        for (path, e) in skipped.iter() {
            let _ = writeln!(so, "略過 {}：{}", path.display(), e);
        }
        let _ = std::io::Write::flush(&mut so);
    }
    let skipped_count = skipped.len();
    // 後續只認成功開檔的清單：索引與 jobs/bases 對齊。
    let dats = ok_dats;

    // 所有 .dat 的 entry 攤平後的一筆工作。
    struct Work {
        dat: usize,
        entry: usize,
        dest: std::path::PathBuf,
    }

    let mut works: Vec<Work> = Vec::new();
    // 總位元組同時是進度條的分母：來自不可信輸入，一律用 checked 加法。
    let mut total: u64 = 0;
    for (di, job) in jobs.iter().enumerate() {
        // 內部不變量：`dats` 與 `bases` 和 `jobs` 同長度、同一迴圈建出來。
        let dat_path = dats
            .get(di)
            .expect("invariant: dats/jobs/bases built in lockstep");
        let base = bases
            .get(di)
            .expect("invariant: dats/jobs/bases built in lockstep");
        for ei in 0..job.len() {
            let (name, entry) = job.get(ei);
            // 任何名稱不安全就在寫檔前整體失敗，避免抽一半留下半成品輸出樹。
            let dest = base.join(crate::pathsafe::safe_relative_path(name)?);
            total = total.checked_add(entry.len as u64).ok_or_else(|| {
                Error::BadFormat(format!(
                    "total payload size overflows u64 at '{}' entry {} ('{}')",
                    dat_path.display(),
                    ei,
                    name
                ))
            })?;
            works.push(Work {
                dat: di,
                entry: ei,
                dest,
            });
        }
    }

    // 資料夾模式共用同一條進度條：所有 .dat 的 entry 在同一個 pool、
    // 同一個 total、同一個 Progress 下跑完，不要為每個 .dat 各建一條。
    let progress = Progress::new(total, opts.quiet);
    crate::pool::for_each_with(
        works.len(),
        opts.threads,
        || Vec::<u8>::new(),
        |buf, i| {
            // pool 保證 `i < works.len()`，但此處仍回錯誤不 panic。
            let w = works.get(i).ok_or_else(|| {
                Error::BadFormat(format!(
                    "internal work index {} out of range (len {})",
                    i,
                    works.len()
                ))
            })?;
            // `Work.dat` 建表時來自合法範圍；對不上是程式錯誤，標 invariant。
            let job = jobs
                .get(w.dat)
                .expect("invariant: Work.dat < jobs.len()");
            let dat_path = dats
                .get(w.dat)
                .expect("invariant: Work.dat < dats.len()");
            // `DatFile::get` 越界會 panic，改走切片 `get` 把壞索引轉成可回報的錯誤。
            let entry_len = job
                .entries()
                .get(w.entry)
                .ok_or_else(|| {
                    Error::BadFormat(format!(
                        "entry {} of '{}' out of range",
                        w.entry,
                        dat_path.display()
                    ))
                })?
                .len;
            let entry_name = job
                .names()
                .get(w.entry)
                .ok_or_else(|| {
                    Error::BadFormat(format!(
                        "entry {} of '{}' out of range",
                        w.entry,
                        dat_path.display()
                    ))
                })?;
            // 讀取失敗直接上拋，保留原始 kind（可能是 Io 或 BadFormat）。
            job.read_entry_into(w.entry, buf)?;
            if let Some(parent) = w.dest.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        Error::Io(std::io::Error::new(
                            e.kind(),
                            format!(
                                "entry {} ('{}') of '{}': cannot create dir '{}': {}",
                                w.entry,
                                entry_name,
                                dat_path.display(),
                                parent.display(),
                                e
                            ),
                        ))
                    })?;
                }
            }
            // 一次性寫入整個 payload：`File::create` + `write_all`，
            // 多一層 `BufWriter` 只是多一次拷貝，沒有好處。
            std::fs::File::create(&w.dest)
                .map_err(|e| {
                    Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "entry {} ('{}') of '{}': cannot create file '{}': {}",
                            w.entry,
                            entry_name,
                            dat_path.display(),
                            w.dest.display(),
                            e
                        ),
                    ))
                })?
                .write_all(buf)
                .map_err(|e| {
                    Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "entry {} ('{}') of '{}': cannot write file '{}': {}",
                            w.entry,
                            entry_name,
                            dat_path.display(),
                            w.dest.display(),
                            e
                        ),
                    ))
                })?;
            // 工作執行緒只做原子累加、不印任何東西；渲染由搶到渲染權的執行緒負責。
            progress.add(entry_len as u64);
            Ok(())
        },
    )?;
    progress.finish();

    Ok(Stats {
        dats: dats.len(),
        files: works.len(),
        bytes: total,
        elapsed: start.elapsed(),
        skipped: skipped_count,
    })
}

/// 收集 `root` 底下（遞迴）所有 .dat。`root` 本身是檔案時只回它一個。
/// 副檔名不分大小寫。回傳的順序必須排序過（決定性）。
fn collect_dats(root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    // 不存在時 `metadata` 的 Io 錯誤直接上拋，呼叫端視為 Io。
    let meta = std::fs::metadata(root)?;
    if meta.is_file() {
        if !is_dat_name(root) {
            return Err(Error::BadFormat(format!(
                "not a .dat file: '{}'",
                root.display()
            )));
        }
        return Ok(vec![root.to_path_buf()]);
    }
    if !meta.is_dir() {
        // 裝置檔等既非檔案也非目錄：無法走訪，也無法當 .dat 開。
        return Err(Error::BadFormat(format!(
            "not a file or directory: '{}'",
            root.display()
        )));
    }
    // 顯式堆疊而非遞迴函式：深度來自不可信的目錄結構，不吃呼叫堆疊。
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            // `file_type` 不跟隨符號連結：連結本身既非 dir 也非 file，
            // 直接略過才不會在連結成環時無限打轉。
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && is_dat_name(&path) {
                found.push(path);
            }
        }
    }
    if found.is_empty() {
        return Err(Error::BadFormat(format!(
            "no .dat found under '{}'",
            root.display()
        )));
    }
    // 決定性順序：走訪順序依檔案系統而異，排序後才穩定。
    found.sort();
    Ok(found)
}

/// 副檔名是否為 `.dat`（不分大小寫）。非 UTF-8 副檔名視為不是。
fn is_dat_name(p: &std::path::Path) -> bool {
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) => e.eq_ignore_ascii_case("dat"),
        None => false,
    }
}

/// 去掉 `.dat` 後的檔名主體。檔名就是 `.dat`（沒有真正的主體，
/// `file_stem` 會原樣回傳）時視為取不到，回 `Error::BadFormat`。
fn stem_of(dat: &std::path::Path) -> Result<&std::ffi::OsStr> {
    let msg = || format!("cannot derive output dir from '{}'", dat.display());
    let name = dat.file_name().ok_or_else(|| Error::BadFormat(msg()))?;
    let stem = dat.file_stem().ok_or_else(|| Error::BadFormat(msg()))?;
    // 先導點開頭且 stem 與全名相同（例如 `.dat`）表示根本沒有主體。
    let bare_dotfile = name == stem && name.to_str().is_some_and(|s| s.starts_with('.'));
    if stem.is_empty() || bare_dotfile {
        return Err(Error::BadFormat(msg()));
    }
    Ok(stem)
}

/// `X.dat` → 與它同目錄下的 `X/`（去掉 `.dat`，大小寫依原檔名）。
fn default_out_dir(dat: &std::path::Path) -> Result<std::path::PathBuf> {
    let stem = stem_of(dat)?;
    // `parent` 為空（例如輸入就是裸檔名）時從空路徑開始 push，結果仍是相對路徑。
    let mut o = dat.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    o.push(stem);
    Ok(o)
}

/// 單一 `.dat` 檔搭配 `-o OUT`：輸出到 `OUT/<stem>/`。
fn single_out_dir(
    dat: &std::path::Path,
    out_root: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let stem = stem_of(dat)?;
    let mut o = out_root.to_path_buf();
    o.push(stem);
    Ok(o)
}

/// 資料夾模式搭配 `-o OUT`：鏡射來源相對結構，避免同名 `.dat` 互撞。
/// `R/a/x.dat` 配掃描根 `R` → `OUT/a/x/`。
fn mirror_out_dir(
    scan_root: &std::path::Path,
    dat: &std::path::Path,
    out_root: &std::path::Path,
) -> Result<std::path::PathBuf> {
    // `dat` 是走訪 `scan_root` 長出來的，前綴一定對得上；對不上是壞輸入，回錯誤。
    let rel = dat.strip_prefix(scan_root).map_err(|_| {
        Error::BadFormat(format!(
            "'{}' is not under scan root '{}'",
            dat.display(),
            scan_root.display()
        ))
    })?;
    let stem = stem_of(rel)?;
    // 只去掉最後一層副檔名（`archive.tar.dat` → `archive.tar`），與 `file_stem` 語意一致。
    let mut o = out_root.to_path_buf();
    if let Some(parent) = rel.parent() {
        if !parent.as_os_str().is_empty() {
            o.push(parent);
        }
    }
    o.push(stem);
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

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

    /// 每個測試一個獨立暫存子目錄；結束後整個刪掉，不留產生物到 repo。
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aokana-extract-unit-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("invariant: test setup must create temp dir");
        p
    }

    /// 在記憶體組出最小合法 .dat（做法同 `dat.rs` 測試的組裝：entry 表與
    /// 名稱表各自加密、payload 各自加密，header 回填 wrapping 總和）。
    /// 為什麼加密傳 Aokana：合成語料沿用原本常數（總和由位移 16 起算 251 項），
    /// 即 Aokana 變體；若將來要測 EXTRA2 合成語料再另傳變體參數。
    fn build_dat(names: &[&str], payloads: &[Vec<u8>]) -> Vec<u8> {
        assert_eq!(names.len(), payloads.len());
        let entry_key = 0x11u32;
        let name_key = 0x22u32;
        let n = names.len();
        let mut name_table = Vec::new();
        let mut offs: Vec<u32> = Vec::with_capacity(n);
        for nm in names {
            offs.push(name_table.len() as u32);
            name_table.extend_from_slice(nm.as_bytes());
            name_table.push(0);
        }
        let data_start =
            crate::dat::HEADER_LEN + crate::dat::ENTRY_LEN * n + name_table.len();
        let mut table = Vec::with_capacity(crate::dat::ENTRY_LEN * n);
        let mut off = data_start;
        for (i, p) in payloads.iter().enumerate() {
            let key = 0x1234_0000u32.wrapping_add(i as u32);
            table.extend_from_slice(&(p.len() as u32).to_le_bytes());
            table.extend_from_slice(&offs[i].to_le_bytes());
            table.extend_from_slice(&key.to_le_bytes());
            table.extend_from_slice(&(off as u32).to_le_bytes());
            off += p.len();
        }
        let mut et = table.clone();
        crate::cri::encrypt_in_place(&mut et, entry_key, crate::cri::Variant::Aokana);
        let mut nt = name_table.clone();
        crate::cri::encrypt_in_place(&mut nt, name_key, crate::cri::Variant::Aokana);
        // header：先放兩把金鑰，再回填最後一格使 wrapping 總和等於 N。
        let mut header = [0u8; crate::dat::HEADER_LEN];
        header[92..96].copy_from_slice(&name_key.to_le_bytes());
        header[212..216].copy_from_slice(&entry_key.to_le_bytes());
        let mut acc: u32 = 0;
        for i in 0..251 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&header[16 + i * 4..16 + i * 4 + 4]);
            acc = acc.wrapping_add(u32::from_le_bytes(b));
        }
        let last = (n as u32).wrapping_sub(acc);
        header[1016..1020].copy_from_slice(&last.to_le_bytes());
        let mut out = Vec::with_capacity(off);
        out.extend_from_slice(&header);
        out.extend_from_slice(&et);
        out.extend_from_slice(&nt);
        for (i, p) in payloads.iter().enumerate() {
            let key = 0x1234_0000u32.wrapping_add(i as u32);
            let mut enc = p.clone();
            crate::cri::encrypt_in_place(&mut enc, key, crate::cri::Variant::Aokana);
            out.extend_from_slice(&enc);
        }
        out
    }

    #[test]
    fn collect_single_dat_file_returns_itself() {
        let root = tmp_root("collect-single");
        let dat = root.join("x.dat");
        std::fs::write(&dat, build_dat(&["a.bin"], &[b"hi".to_vec()]))
            .expect("invariant: test setup must write temp dat");
        let got = collect_dats(&dat).expect("single .dat must collect");
        assert_eq!(got, vec![dat]);
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
    }

    #[test]
    fn collect_nested_dir_is_sorted_and_case_insensitive() {
        let root = tmp_root("collect-nested");
        let sub = root.join("sub");
        let deep = sub.join("deep");
        std::fs::create_dir_all(&deep).expect("invariant: test setup must make dirs");
        // 大寫副檔名也要收；非 .dat 一律略過。
        let files: &[(&str, bool)] = &[
            ("b.DAT", true),
            ("sub/a.dat", true),
            ("sub/deep/d.dat", true),
            ("ignore.bin", false),
            ("sub/skip.txt", false),
        ];
        for (rel, _) in files {
            std::fs::write(
                root.join(rel),
                build_dat(&["a.bin"], &[b"x".to_vec()]),
            )
            .expect("invariant: test setup must write temp dat");
        }
        let mut want: Vec<std::path::PathBuf> = files
            .iter()
            .filter(|(_, keep)| *keep)
            .map(|(rel, _)| root.join(rel))
            .collect();
        want.sort();
        let got = collect_dats(&root).expect("nested dir must collect");
        assert_eq!(got, want);
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
    }

    #[test]
    fn collect_dir_without_dat_is_bad_format() {
        let root = tmp_root("collect-empty");
        std::fs::write(root.join("note.txt"), b"no dat here")
            .expect("invariant: test setup must write temp file");
        let err = collect_dats(&root).err().expect("dir without .dat must fail");
        match err {
            Error::BadFormat(m) => assert!(m.contains("no .dat found"), "unexpected: {m}"),
            other => panic!("expected BadFormat, got {other:?}"),
        }
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
    }

    #[test]
    fn collect_non_dat_file_is_bad_format() {
        let root = tmp_root("collect-nondat");
        let f = root.join("note.txt");
        std::fs::write(&f, b"plain text").expect("invariant: test setup must write temp file");
        let err = collect_dats(&f).err().expect("non-.dat file must fail");
        match err {
            Error::BadFormat(m) => assert!(m.contains("not a .dat file"), "unexpected: {m}"),
            other => panic!("expected BadFormat, got {other:?}"),
        }
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
    }

    #[test]
    fn out_dir_naming_strips_dat_suffix() {
        let root = tmp_root("naming");
        std::fs::create_dir_all(root.join("a")).expect("invariant: test setup must make dir");
        // `X.dat` → 同目錄下的 `X/`，大小寫依原檔名。
        assert_eq!(
            default_out_dir(&root.join("X.dat")).expect("X.dat must name X"),
            root.join("X")
        );
        assert_eq!(
            default_out_dir(&root.join("Y.DAT")).expect("Y.DAT must name Y"),
            root.join("Y")
        );
        // 單一檔 + `-o OUT` → `OUT/<stem>/`。
        assert_eq!(
            single_out_dir(&root.join("X.dat"), &root.join("OUT")).expect("single must join stem"),
            root.join("OUT").join("X")
        );
        // 資料夾 + `-o OUT` 鏡射相對結構：同名不同目錄不互撞。
        assert_eq!(
            mirror_out_dir(&root, &root.join("a/x.dat"), &root.join("OUT"))
                .expect("mirror must keep subdir"),
            root.join("OUT").join("a").join("x")
        );
        assert_eq!(
            mirror_out_dir(&root, &root.join("x.dat"), &root.join("OUT"))
                .expect("mirror of top-level dat"),
            root.join("OUT").join("x")
        );
        // stem 取不到（檔名就是 `.dat`）→ BadFormat。
        let err = default_out_dir(&root.join(".dat"))
            .err()
            .expect(".dat must fail naming");
        assert!(
            matches!(err, Error::BadFormat(_)),
            "expected BadFormat, got {err:?}"
        );
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
    }

    #[test]
    fn dir_with_unreadable_dat_skips_it_but_succeeds() {
        // 資料夾裡混著打不開的 .dat：能開的照解、打不開的計數略過；
        // 全部打不開才整體失敗。
        let root = tmp_root("skip-unreadable");
        std::fs::write(
            root.join("good.dat"),
            build_dat(&["a.bin"], &[b"hello".to_vec()]),
        )
        .expect("invariant: test setup must write temp dat");
        // 開檔只看 header 就會失敗：長度連 header 都不到。
        std::fs::write(root.join("bad.dat"), vec![0xA5u8; 64])
            .expect("invariant: test setup must write bad dat");
        let opts = Options {
            threads: 1,
            quiet: true,
        };
        let stats = run(&root, None, &opts).expect("mixed dir must succeed");
        assert_eq!(stats.dats, 1);
        assert_eq!(stats.skipped, 1);
        let got = std::fs::read(root.join("good").join("a.bin"))
            .expect("invariant: good.dat entry must be extracted");
        assert_eq!(got, b"hello");

        let only_bad = tmp_root("skip-only-bad");
        std::fs::write(only_bad.join("bad.dat"), vec![0xA5u8; 64])
            .expect("invariant: test setup must write bad dat");
        assert!(
            run(&only_bad, None, &opts).is_err(),
            "dir with only unreadable .dat must fail"
        );
        assert!(std::fs::remove_dir_all(&root).is_ok(), "cleanup temp dir");
        assert!(std::fs::remove_dir_all(&only_bad).is_ok(), "cleanup temp dir");
    }
}
