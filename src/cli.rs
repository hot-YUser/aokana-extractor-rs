//! 手寫參數解析與使用說明。不引入 clap 是為了零依賴。

use std::path::PathBuf;

use crate::error::Result;

pub enum Command {
    /// 拖拽模式：無子命令，只有一個位置參數。
    /// 與 `Extract` 唯一的差別是失敗時會暫停等按鍵——否則從檔案總管拖進來時，
    /// 主控台視窗會在錯誤訊息被看到之前就關閉。
    Drag { path: PathBuf },
    Extract { path: PathBuf, out: Option<PathBuf> },
    List { dat: PathBuf },
    Help,
    Version,
}

pub struct Cli {
    pub command: Command,
    pub threads: usize, // 0 = 自動
    pub quiet: bool,
}

pub const USAGE: &str = "aokana — Aokana (蒼の彼方のフォーリズム) Steam .dat 拆包解密工具

用法：
  aokana <path>                      拖拽模式：.dat → 解到同目錄的同名資料夾；
                                     資料夾 → 遞迴解開其中所有 .dat
  aokana extract <path> [-o <dir>]   同上，可指定輸出根目錄
  aokana list <file.dat>             列出內容

選項：
  -o, --out <dir>     輸出根目錄（預設：與 .dat 同目錄）
  -j, --threads <N>   執行緒數，0 表示自動（預設 0）
  -q, --quiet         不顯示進度
  -h, --help
  -V, --version
";

/// 手寫解析，支援 `--opt value`、`--opt=value`、`-j8`、`-j 8`、`-oout`、`-o out` 四種形式。
/// 解析失敗時回傳人類可讀的錯誤訊息字串（含 USAGE），由 main 印到 stderr 並以 exit code 2 結束。
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> core::result::Result<Cli, String> {
    let args: Vec<String> = args.into_iter().collect();
    // 為什麼先收成 Vec：取值選項需要向前看下一個 token，迭代器沒有預覽會很難寫。
    let mut raw = Raw::default();
    let mut i = 0usize;
    while i < args.len() {
        let tok = match args.get(i) {
            Some(t) => t.as_str(),
            // 內部不變量：i < len 已檢查，get 必定成功；此分支不可達。
            None => return Err(fail("internal error: argument index out of range")),
        };
        if tok == "--" {
            // 之後的全是位置參數（檔名可能以 '-' 開頭）。
            let mut k = i + 1;
            while k < args.len() {
                if let Some(t) = args.get(k) {
                    push_positional(&mut raw, t.clone())?;
                }
                k += 1;
            }
            break;
        } else if let Some(rest) = tok.strip_prefix("--") {
            // 長選項：--opt value 或 --opt=value。
            let (name, inline) = match rest.find('=') {
                Some(eq) => {
                    let (n, v) = rest.split_at(eq);
                    // split_at 切在 '=' 上，v 以 '=' 開頭，去掉它。
                    (n, Some(v.get(1..).unwrap_or("")))
                }
                None => (rest, None),
            };
            match name {
                "threads" => {
                    let v = value_of(&args, &mut i, inline, "--threads")?;
                    raw.threads = Some(parse_threads(&v)?);
                }
                "out" => {
                    let v = value_of(&args, &mut i, inline, "--out")?;
                    raw.out = Some(v);
                }
                "quiet" => {
                    reject_inline("--quiet", inline)?;
                    raw.quiet = true;
                }
                "help" => {
                    reject_inline("--help", inline)?;
                    raw.help = true;
                }
                "version" => {
                    reject_inline("--version", inline)?;
                    raw.version = true;
                }
                // 已移除的選項一律走未知選項錯誤，不靜默忽略。
                _ => return Err(fail(format!("unknown option: --{name}"))),
            }
            i += 1;
        } else if tok.starts_with('-') && tok.len() > 1 {
            // 短選項：-j8 / -j 8 / -oout / -o out / -q / -h / -V。
            let short = match tok.get(1..2) {
                Some(s) => s,
                None => return Err(fail(format!("unknown option: {tok}"))),
            };
            match short {
                "j" => {
                    let attached = tok.get(2..).unwrap_or("");
                    let v = if attached.is_empty() {
                        next_value(&args, &mut i, "-j")?
                    } else {
                        // 接受 -j=8 這種混合寫法，去掉可選的 '='。
                        attached.strip_prefix('=').unwrap_or(attached).to_string()
                    };
                    raw.threads = Some(parse_threads(&v)?);
                    i += 1;
                }
                "o" => {
                    let attached = tok.get(2..).unwrap_or("");
                    let v = if attached.is_empty() {
                        next_value(&args, &mut i, "-o")?
                    } else {
                        attached.strip_prefix('=').unwrap_or(attached).to_string()
                    };
                    if v.is_empty() {
                        return Err(fail("option -o requires a value"));
                    }
                    raw.out = Some(v);
                    i += 1;
                }
                "q" => {
                    if tok.len() != 2 {
                        return Err(fail(format!("unknown option: {tok}")));
                    }
                    raw.quiet = true;
                    i += 1;
                }
                "h" => {
                    if tok.len() != 2 {
                        return Err(fail(format!("unknown option: {tok}")));
                    }
                    raw.help = true;
                    i += 1;
                }
                "V" => {
                    if tok.len() != 2 {
                        return Err(fail(format!("unknown option: {tok}")));
                    }
                    raw.version = true;
                    i += 1;
                }
                // 已移除的 -d 與其他短選項一律走未知選項錯誤。
                _ => return Err(fail(format!("unknown option: {tok}"))),
            }
        } else if raw.sub.is_none() && is_subcommand(tok) {
            raw.sub = Some(tok.to_string());
            i += 1;
        } else {
            raw.positionals.push(tok.to_string());
            i += 1;
        }
    }

    let threads = raw.threads.unwrap_or(0);
    let quiet = raw.quiet;

    // -h / --help 與 -V / --version 優先於一切，直接回傳。
    if raw.help || raw.sub.as_deref() == Some("help") {
        return Ok(Cli {
            command: Command::Help,
            threads,
            quiet,
        });
    }
    if raw.version || raw.sub.as_deref() == Some("version") {
        return Ok(Cli {
            command: Command::Version,
            threads,
            quiet,
        });
    }

    // 為什麼先 clone 子命令：後續分支會搬移 raw 的欄位，
    // 若直接 match raw.sub.as_deref() 會借用 raw 而與搬移衝突。
    let sub = raw.sub.clone();
    // 為什麼 -o 只認 extract：Drag 沒有輸出根目錄的概念（永遠解到 .dat 旁），
    // list 也不寫檔；出現在別處一律當未知選項，與 --dry-run 等已移除選項同處理。
    if raw.out.is_some() && !matches!(sub.as_deref(), Some("extract")) {
        return Err(fail("unknown option: -o/--out is only valid with `extract`"));
    }
    let command = match sub.as_deref() {
        None => {
            // 拖拽模式：無子命令、只有一個位置參數。檔案或資料夾皆可，
            // 不檢查副檔名（資料夾本來就沒有副檔名）；舊版「等同 list」行為已取消。
            match raw.positionals.len() {
                0 => return Err(fail("missing arguments")),
                1 => {
                    let path = position_at(&raw, 0, "<path>")?;
                    Command::Drag { path }
                }
                _ => {
                    // 為什麼保留子命令拼錯提示：首參數不像 .dat 時極可能是打錯的子命令
                    //（舊行為）；資料夾拖拽只會有一個參數，走不到這裡，不受影響。
                    if let Some(first) = raw.positionals.first() {
                        let is_dat = std::path::Path::new(first)
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|e| e.eq_ignore_ascii_case("dat"))
                            .unwrap_or(false);
                        if !is_dat {
                            return Err(fail(format!("unknown subcommand: {first}")));
                        }
                    }
                    return Err(fail(format!(
                        "too many arguments: expected 1, got {}",
                        raw.positionals.len()
                    )));
                }
            }
        }
        Some("list") => {
            require_positionals(&raw, 1, "aokana list <file.dat>")?;
            Command::List {
                dat: position_at(&raw, 0, "<file.dat>")?,
            }
        }
        Some("extract") => {
            require_positionals(&raw, 1, "aokana extract <path> [-o <dir>]")?;
            Command::Extract {
                path: position_at(&raw, 0, "<path>")?,
                out: raw.out.map(PathBuf::from),
            }
        }
        Some(other) => return Err(fail(format!("unknown subcommand: {other}"))),
    };
    Ok(Cli {
        command,
        threads,
        quiet,
    })
}

/// 分派子命令。`Help` / `Version` 寫到 `out`。
/// `Drag` / `Extract` 解包成功且非 quiet 時印出一行摘要（§5.4）。
pub fn run(cli: Cli, out: &mut impl std::io::Write, _err: &mut impl std::io::Write) -> Result<()> {
    match cli.command {
        Command::Help => {
            out.write_all(USAGE.as_bytes())?;
            Ok(())
        }
        Command::Version => {
            // 為什麼用 env!：版本來自 Cargo.toml 的唯一真相，不手寫重複數字。
            writeln!(out, "{}", env!("CARGO_PKG_VERSION"))?;
            Ok(())
        }
        Command::List { dat } => {
            let d = crate::dat::DatFile::open(&dat)?;
            crate::extract::list(&d, out)
        }
        Command::Drag { path } => {
            let opts = crate::extract::Options {
                threads: cli.threads,
                quiet: cli.quiet,
            };
            let stats = crate::extract::run(&path, None, &opts)?;
            print_summary(out, cli.quiet, &stats)?;
            Ok(())
        }
        Command::Extract { path, out: root } => {
            let opts = crate::extract::Options {
                threads: cli.threads,
                quiet: cli.quiet,
            };
            let stats = crate::extract::run(&path, root.as_deref(), &opts)?;
            print_summary(out, cli.quiet, &stats)?;
            Ok(())
        }
    }
}

/// 解包完成後的摘要行（與進度條無關）。`-q` 時連這行也不印。
fn print_summary(
    w: &mut impl std::io::Write,
    quiet: bool,
    s: &crate::extract::Stats,
) -> Result<()> {
    // 為什麼摘要與終端機無關：重導向時進度條停用，但摘要仍是機器可讀的一行結論。
    if !quiet {
        if s.skipped == 0 {
            writeln!(
                w,
                "已解包 {} 個 .dat、{} 個檔案、{}，耗時 {} 秒",
                s.dats,
                s.files,
                format_bytes(s.bytes),
                format_secs(s.elapsed),
            )?;
        } else {
            // 有略過才補尾段：零略過時輸出與以往逐字相同。
            writeln!(
                w,
                "已解包 {} 個 .dat、{} 個檔案、{}，耗時 {} 秒（略過 {} 個無法讀取的 .dat）",
                s.dats,
                s.files,
                format_bytes(s.bytes),
                format_secs(s.elapsed),
                s.skipped,
            )?;
        }
    }
    Ok(())
}

/// 位元組格式化（與進度條同規則）：1024 進位，G/M/K 取一位小數並去尾 ".0"，B 取整數。
fn format_bytes(n: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * 1024 * 1024;
    if n >= G {
        trim1(n as f64 / G as f64, "G")
    } else if n >= M {
        trim1(n as f64 / M as f64, "M")
    } else if n >= K {
        trim1(n as f64 / K as f64, "K")
    } else {
        format!("{n}B")
    }
}

fn trim1(v: f64, unit: &str) -> String {
    // 為什麼去尾 ".0"：整數倍時 "1M" 比 "1.0M" 好讀，與規格範例一致。
    let s = format!("{v:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}{unit}")
}

/// 耗時取一位小數的秒數，例如 6.1。
fn format_secs(d: std::time::Duration) -> String {
    format!("{:.1}", d.as_secs_f64())
}

#[derive(Default)]
struct Raw {
    threads: Option<usize>,
    quiet: bool,
    help: bool,
    version: bool,
    out: Option<String>,
    sub: Option<String>,
    positionals: Vec<String>,
}

fn fail(msg: impl Into<String>) -> String {
    // 為什麼把 USAGE 接在後面：呼叫端直接把整個字串印到 stderr，exit code 2。
    format!("error: {}\n\n{USAGE}", msg.into())
}

fn is_subcommand(tok: &str) -> bool {
    matches!(tok, "list" | "extract" | "help" | "version")
}

fn push_positional(raw: &mut Raw, tok: String) -> core::result::Result<(), String> {
    // "--" 之後的不再判子命令：檔名恰好叫 "list" 也不該被誤認。
    raw.positionals.push(tok);
    Ok(())
}

/// 取 `--opt`（無 `=`) 的下一個 token 當值；沒有就報錯。
fn next_value(args: &[String], i: &mut usize, opt: &str) -> core::result::Result<String, String> {
    match args.get(*i + 1) {
        Some(v) => {
            *i += 1;
            Ok(v.clone())
        }
        None => Err(fail(format!("option {opt} requires a value"))),
    }
}

/// `--opt value` 與 `--opt=value` 共用：有內嵌值就用它，否則吃下一個 token。
fn value_of(
    args: &[String],
    i: &mut usize,
    inline: Option<&str>,
    opt: &str,
) -> core::result::Result<String, String> {
    if let Some(v) = inline {
        if v.is_empty() {
            return Err(fail(format!("option {opt} requires a value")));
        }
        return Ok(v.to_string());
    }
    next_value(args, i, opt)
}

/// bool 選項不接受 `=value` 形式。
fn reject_inline(opt: &str, inline: Option<&str>) -> core::result::Result<(), String> {
    if inline.is_some() {
        return Err(fail(format!("option {opt} takes no value")));
    }
    Ok(())
}

fn parse_threads(v: &str) -> core::result::Result<usize, String> {
    // 為什麼不用 unwrap：外部輸入 "abc" 必須變成含 USAGE 的 Err，而不是 panic。
    match v.parse::<usize>() {
        Ok(n) => Ok(n),
        Err(_) => Err(fail(format!("invalid --threads value: {v}"))),
    }
}

fn position_at(raw: &Raw, idx: usize, what: &str) -> core::result::Result<PathBuf, String> {
    match raw.positionals.get(idx) {
        Some(p) => Ok(PathBuf::from(p)),
        None => Err(fail(format!("missing {what}"))),
    }
}

fn require_positionals(raw: &Raw, n: usize, usage: &str) -> core::result::Result<(), String> {
    if raw.positionals.len() < n {
        return Err(fail(format!("missing arguments\nusage: {usage}")));
    }
    if raw.positionals.len() > n {
        return Err(fail(format!(
            "too many arguments: expected {n}, got {}",
            raw.positionals.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn parse_str(v: &[&str]) -> core::result::Result<Cli, String> {
        parse(args(v))
    }

    fn must_ok(v: &[&str]) -> Cli {
        match parse_str(v) {
            Ok(c) => c,
            Err(e) => panic!("invariant: fixed test input should parse: {e}"),
        }
    }

    fn must_err(v: &[&str]) -> String {
        match parse_str(v) {
            Ok(_) => panic!("invariant: test input should fail to parse"),
            Err(e) => e,
        }
    }

    fn drag_path(cli: &Cli) -> &PathBuf {
        match &cli.command {
            Command::Drag { path } => path,
            _ => panic!("invariant: expected Drag command"),
        }
    }

    #[test]
    fn drag_single_dat() {
        let cli = must_ok(&["a.dat"]);
        assert_eq!(drag_path(&cli), &PathBuf::from("a.dat"));
        assert_eq!(cli.threads, 0);
        assert!(!cli.quiet);
    }

    #[test]
    fn drag_accepts_folder_without_dat_extension() {
        // 資料夾本來就沒有副檔名：拖拽模式不檢查副檔名。
        let cli = must_ok(&["somedir"]);
        assert_eq!(drag_path(&cli), &PathBuf::from("somedir"));
    }

    #[test]
    fn drag_missing_arguments() {
        let err = must_err(&[]);
        assert!(err.contains("missing"), "{err}");
        assert!(err.contains("用法"), "{err}");
    }

    #[test]
    fn drag_two_positionals_fail() {
        // 舊相容用法 `aokana <file.dat> <out>` 已取消：無子命令只接受一個位置參數。
        let err = must_err(&["a.dat", "out"]);
        assert!(err.contains("too many"), "{err}");
        assert!(err.contains("用法"), "{err}");
    }

    #[test]
    fn drag_with_out_fails_as_unknown_option() {
        // -o 只在 extract 下有意義，拖拽模式出現要當未知選項。
        for v in [
            vec!["a.dat", "-o", "out"],
            vec!["--out=out", "a.dat"],
            vec!["a.dat", "--out", "out"],
            vec!["somedir", "-oout"],
        ] {
            let err = must_err(&v);
            assert!(err.contains("unknown option"), "{v:?}: {err}");
            assert!(err.contains("用法"), "{v:?}: {err}");
        }
    }

    #[test]
    fn extract_defaults_to_none_out() {
        let cli = must_ok(&["extract", "a.dat"]);
        match &cli.command {
            Command::Extract { path, out } => {
                assert_eq!(path, &PathBuf::from("a.dat"));
                assert!(out.is_none());
            }
            _ => panic!("invariant: expected Extract command"),
        }
    }

    #[test]
    fn extract_out_forms() {
        for v in [
            vec!["extract", "a.dat", "-o", "out"],
            vec!["extract", "a.dat", "-oout"],
            vec!["extract", "a.dat", "--out", "out"],
            vec!["extract", "a.dat", "--out=out"],
            vec!["-o", "out", "extract", "a.dat"],
        ] {
            let cli = must_ok(&v);
            match &cli.command {
                Command::Extract { out, .. } => {
                    assert_eq!(out.as_ref(), Some(&PathBuf::from("out")), "{v:?}")
                }
                _ => panic!("invariant: expected Extract command for {v:?}"),
            }
        }
    }

    #[test]
    fn extract_old_two_positional_form_fails() {
        // 舊 `extract <file.dat> <out_dir>` 已改為 `-o`：第二個位置參數現在是多餘的。
        let err = must_err(&["extract", "a.dat", "out"]);
        assert!(err.contains("too many"), "{err}");
    }

    #[test]
    fn extract_missing_path_fails() {
        let err = must_err(&["extract"]);
        assert!(err.contains("missing"), "{err}");
        assert!(err.contains("用法"), "{err}");
    }

    #[test]
    fn list_parses() {
        let cli = must_ok(&["list", "a.dat"]);
        assert!(matches!(cli.command, Command::List { .. }));
        assert_eq!(cli.threads, 0);
        assert!(!cli.quiet);
    }

    #[test]
    fn list_with_out_fails_as_unknown_option() {
        let err = must_err(&["list", "a.dat", "-o", "o"]);
        assert!(err.contains("unknown option"), "{err}");
    }

    #[test]
    fn threads_forms() {
        // --threads 8
        let cli = must_ok(&["-j", "8", "list", "a.dat"]);
        assert_eq!(cli.threads, 8);
        // --threads=8
        let cli = must_ok(&["list", "a.dat", "--threads=8"]);
        assert_eq!(cli.threads, 8);
        // -j8
        let cli = must_ok(&["-j8", "a.dat"]);
        assert_eq!(cli.threads, 8);
        assert!(matches!(cli.command, Command::Drag { .. }));
        // --threads 8（子命令之後）
        let cli = must_ok(&["extract", "a.dat", "--threads", "4"]);
        assert_eq!(cli.threads, 4);
    }

    #[test]
    fn quiet_short_and_long() {
        let cli = must_ok(&["-q", "list", "a.dat"]);
        assert!(cli.quiet);
        let cli = must_ok(&["list", "a.dat", "--quiet"]);
        assert!(cli.quiet);
        let cli = must_ok(&["-q", "a.dat"]);
        assert!(cli.quiet);
        assert!(matches!(cli.command, Command::Drag { .. }));
    }

    #[test]
    fn help_and_version() {
        let cli = must_ok(&["-h"]);
        assert!(matches!(cli.command, Command::Help));
        let cli = must_ok(&["--help"]);
        assert!(matches!(cli.command, Command::Help));
        let cli = must_ok(&["help"]);
        assert!(matches!(cli.command, Command::Help));
        let cli = must_ok(&["-V"]);
        assert!(matches!(cli.command, Command::Version));
        let cli = must_ok(&["--version"]);
        assert!(matches!(cli.command, Command::Version));
        let cli = must_ok(&["version"]);
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn removed_options_fail_as_unknown() {
        // 已移除的選項不得靜默忽略，一律走未知選項錯誤。
        for v in [
            vec!["extract", "a.dat", "--dry-run"],
            vec!["list", "a.dat", "--dir", "d"],
            vec!["combine", "v.csv"],
            vec!["convert", "d"],
            vec!["info", "a.dat"],
            vec!["list", "a.dat", "--background", "none"],
            vec!["convert", "d", "--ffmpeg", "/bin/ffmpeg"],
            vec!["combine", "v.csv", "-d", "i"],
        ] {
            let err = must_err(&v);
            assert!(err.contains("用法"), "{v:?}: {err}");
        }
    }

    #[test]
    fn unknown_subcommand_fails_with_usage() {
        let err = must_err(&["frobnicate", "a.dat"]);
        assert!(err.contains("unknown subcommand"), "{err}");
        assert!(err.contains("用法"), "{err}");
    }

    #[test]
    fn missing_parameter_fails() {
        let err = must_err(&["list"]);
        assert!(err.contains("missing"), "{err}");
        assert!(err.contains("用法"), "{err}");
        let err = must_err(&["extract", "a.dat", "extra"]);
        assert!(err.contains("too many"), "{err}");
        let err = must_err(&[]);
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn too_many_positionals_fails() {
        let err = must_err(&["list", "a.dat", "extra"]);
        assert!(err.contains("too many"), "{err}");
    }

    #[test]
    fn threads_abc_fails() {
        let err = must_err(&["list", "a.dat", "--threads", "abc"]);
        assert!(err.contains("--threads"), "{err}");
        assert!(err.contains("用法"), "{err}");
    }

    #[test]
    fn unknown_option_fails() {
        let err = must_err(&["list", "a.dat", "--bogus"]);
        assert!(err.contains("unknown option"), "{err}");
        let err = must_err(&["list", "a.dat", "-Z"]);
        assert!(err.contains("unknown option"), "{err}");
    }

    #[test]
    fn missing_option_value_fails() {
        let err = must_err(&["list", "a.dat", "--threads"]);
        assert!(err.contains("requires a value"), "{err}");
        let err = must_err(&["-j"]);
        assert!(err.contains("requires a value"), "{err}");
        let err = must_err(&["extract", "a.dat", "--out"]);
        assert!(err.contains("requires a value"), "{err}");
        let err = must_err(&["extract", "a.dat", "-o"]);
        assert!(err.contains("requires a value"), "{err}");
    }

    #[test]
    fn summary_bytes_format() {
        // 與進度條同規則：規格 §5.2 的取樣點。
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(1024), "1K");
        assert_eq!(format_bytes(1536), "1.5K");
        assert_eq!(format_bytes(1024 * 1024), "1M");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3G");
    }

    #[test]
    fn run_help_and_version_write_to_out() {
        let cli = must_ok(&["-h"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(cli, &mut out, &mut err).expect("invariant: Help always succeeds");
        assert_eq!(out, USAGE.as_bytes());

        let cli = must_ok(&["-V"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(cli, &mut out, &mut err).expect("invariant: Version always succeeds");
        assert_eq!(out, format!("{}\n", env!("CARGO_PKG_VERSION")).as_bytes());
    }
}
