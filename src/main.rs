//! 薄殼：解析參數、分派、決定 exit code。本檔不得 panic。

use aokana_extractor::cli::{parse, run};

fn main() {
    // 為什麼在這裡收集：cli::parse 要擁有 Vec<String>，skip(1) 去掉程式名。
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(args) {
        Ok(cli) => cli,
        // 解析失敗的訊息已含 USAGE，直接印到 stderr，以 exit code 2 結束。
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    // 為什麼這裡不能長期持有 stdout/stderr 的鎖：stdout 的鎖同執行緒可重入、
    // 跨執行緒不可。run() 內部會開工作執行緒平行抽取/轉換，而工作執行緒的
    // 即時進度會呼叫 `stdout().lock()`（短暫取鎖、寫一行、釋放）。若主執行緒
    // 在 run() 期間一直持有鎖，工作執行緒就會永久等待，形成 AB-BA 死鎖
    // （主執行緒等 join、工作執行緒等鎖）。Stdout/Stderr 本身就實作 Write，
    // 每次寫入只短暫取鎖再釋放，所以直接傳未鎖定的握把即可。
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    // run 失敗（I/O、格式錯誤、外部命令失敗等）以 exit code 1 結束。
    if let Err(e) = run(cli, &mut out, &mut err) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let _ = std::io::Write::flush(&mut out);
}
