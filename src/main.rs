//! 薄殼：解析參數、分派、決定 exit code。本檔不得 panic。

use std::io::IsTerminal;

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
    // 為什麼先記下是否拖拽模式：run(cli) 會搬走 cli；只有 Drag 失敗時才暫停等按鍵，
    // 讓從檔案總管拖進來的使用者看得到錯誤訊息（§7.1）。
    let is_drag = matches!(cli.command, aokana_extractor::cli::Command::Drag { .. });
    // 為什麼這裡不能長期持有 stdout/stderr 的鎖：stdout 的鎖同執行緒可重入、
    // 跨執行緒不可。run() 內部會開工作執行緒平行抽取，而進度條渲染會在工作執行緒
    // 裡短暫取 stdout 的鎖。若主執行緒在 run() 期間一直持有鎖，工作執行緒就會永久
    // 等待，形成 AB-BA 死鎖（主執行緒等 join、工作執行緒等鎖）。Stdout/Stderr
    // 本身就實作 Write，每次寫入只短暫取鎖再釋放，所以直接傳未鎖定的握把即可。
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    // run 失敗（I/O、格式錯誤等）以 exit code 1 結束。
    if let Err(e) = run(cli, &mut out, &mut err) {
        eprintln!("{e}");
        if is_drag && std::io::stdout().is_terminal() {
            // 為什麼只看 stdout 是否終端機：管線、重導向、CI 下暫停會卡住自動化；
            // 明確子命令（extract/list）也不暫停，只有 Drag 需要。
            // stdin 讀失敗就忽略，不改變 exit code。
            eprint!("按 Enter 關閉...");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
        std::process::exit(1);
    }
    let _ = std::io::Write::flush(&mut out);
}
