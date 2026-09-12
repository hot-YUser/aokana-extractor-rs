//! 手寫參數解析與使用說明。不引入 clap 是為了零依賴。

use std::path::PathBuf;

use crate::error::Result;

pub enum Command {
    List { dat: PathBuf },
    Extract { dat: PathBuf, out_dir: PathBuf },
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
  aokana list    <file.dat>
  aokana extract <file.dat> <out_dir>

  相容用法（等同上游 extract.py）：
  aokana <file.dat>                → list
  aokana <file.dat> <out_dir>      → extract

全域選項：
  -j, --threads <N>   執行緒數，0 表示自動（預設 0）
  -q, --quiet         不輸出進度
  -h, --help
  -V, --version
";

/// 手寫解析，支援 `--opt value`、`--opt=value`、`-j8`、`-j 8` 四種形式。
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
            // 短選項：-j8 / -j 8 / -q / -h / -V。
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
                // 已移除的 -o / -d 與其他短選項一律走未知選項錯誤。
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
    let command = match sub.as_deref() {
        None => {
            // 相容用法（等同上游 extract.py）：沒有子命令時以位置參數個數分派。
            // 為什麼要求 .dat 後綴：相容用法的本意就是直接吃 .dat 檔；
            // 首參數不是 .dat 時極可能是打錯的子命令，不該靜默當 list 跑。
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
            match raw.positionals.len() {
                0 => return Err(fail("missing arguments")),
                1 => {
                    let dat = position_at(&raw, 0, "<file.dat>")?;
                    Command::List { dat }
                }
                2 => {
                    let dat = position_at(&raw, 0, "<file.dat>")?;
                    let out_dir = position_at(&raw, 1, "<out_dir>")?;
                    Command::Extract { dat, out_dir }
                }
                _ => {
                    return Err(fail(format!(
                        "too many arguments: expected at most 2, got {}",
                        raw.positionals.len()
                    )))
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
            require_positionals(&raw, 2, "aokana extract <file.dat> <out_dir>")?;
            Command::Extract {
                dat: position_at(&raw, 0, "<file.dat>")?,
                out_dir: position_at(&raw, 1, "<out_dir>")?,
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
pub fn run(cli: Cli, out: &mut impl std::io::Write, _err: &mut impl std::io::Write) -> Result<()> {
    // 為什麼 run 不自己印統計：抽取的進度輸出已由 extract 負責；這裡只做分派，保持薄殼。
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
        Command::Extract { dat, out_dir } => {
            let d = crate::dat::DatFile::open(&dat)?;
            let opts = crate::extract::Options {
                threads: cli.threads,
                quiet: cli.quiet,
            };
            let _ = crate::extract::extract(&d, &out_dir, &opts)?;
            Ok(())
        }
    }
}

#[derive(Default)]
struct Raw {
    threads: Option<usize>,
    quiet: bool,
    help: bool,
    version: bool,
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

    #[test]
    fn list_parses() {
        let cli = must_ok(&["list", "a.dat"]);
        assert!(matches!(cli.command, Command::List { .. }));
        assert_eq!(cli.threads, 0);
        assert!(!cli.quiet);
    }

    #[test]
    fn extract_parses() {
        let cli = must_ok(&["extract", "a.dat", "out"]);
        assert!(matches!(cli.command, Command::Extract { .. }));
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
        let cli = must_ok(&["-j8", "list", "a.dat"]);
        assert_eq!(cli.threads, 8);
        // --threads 8（子命令之後）
        let cli = must_ok(&["extract", "a.dat", "out", "--threads", "4"]);
        assert_eq!(cli.threads, 4);
    }

    #[test]
    fn quiet_short_and_long() {
        let cli = must_ok(&["-q", "list", "a.dat"]);
        assert!(cli.quiet);
        let cli = must_ok(&["list", "a.dat", "--quiet"]);
        assert!(cli.quiet);
    }

    #[test]
    fn compat_single_and_double_positional() {
        let cli = must_ok(&["a.dat"]);
        assert!(matches!(cli.command, Command::List { .. }));
        let cli = must_ok(&["a.dat", "out"]);
        assert!(matches!(cli.command, Command::Extract { .. }));
    }

    #[test]
    fn help_and_version() {
        let cli = must_ok(&["-h"]);
        assert!(matches!(cli.command, Command::Help));
        let cli = must_ok(&["--help"]);
        assert!(matches!(cli.command, Command::Help));
        let cli = must_ok(&["-V"]);
        assert!(matches!(cli.command, Command::Version));
        let cli = must_ok(&["--version"]);
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn removed_options_fail_as_unknown() {
        // 已移除的選項不得靜默忽略，一律走未知選項錯誤。
        for v in [
            vec!["extract", "a.dat", "out", "--dry-run"],
            vec!["extract", "a.dat", "out", "--out=o"],
            vec!["list", "a.dat", "--dir", "d"],
            vec!["combine", "v.csv"],
            vec!["convert", "d"],
            vec!["info", "a.dat"],
            vec!["list", "a.dat", "--background", "none"],
            vec!["convert", "d", "--ffmpeg", "/bin/ffmpeg"],
            vec!["combine", "v.csv", "-o", "o"],
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
        let err = must_err(&["extract", "a.dat"]);
        assert!(err.contains("missing"), "{err}");
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
