# aokana

Aokana（蒼の彼方のフォーリズム）Steam 版 `.dat` 拆包解密工具，零依賴 Rust，原生多執行緒。只做拆包解密，不做任何格式轉換。

## 建置

```
cargo build --release
```

實測 `target/release/aokana.exe` 約 195 KiB（199680 位元組）。

## 用法

```
aokana list <file.dat>            # 列出內容
aokana extract <file.dat> <out>   # 解密後原樣寫出
```

範例：

```
aokana list data.dat
aokana extract data.dat ./out -j 4
```

相容用法：`aokana <file.dat>` 等同 list，`aokana <file.dat> <out>` 等同 extract。`-j N` 指定執行緒數（0 表自動），`-q` 不輸出進度，`-h/--help` 顯示說明，`-V/--version` 顯示版本。

## 與上游的差異

- 不做 `adult.dat` 的 `def/version.txt` 移除（上游 `PRead.cs` 會移除；不該靜默丟檔）
- 保留名稱原始大小寫（同上游 `extract.py`）
- 所有損壞輸入回傳明確錯誤，不 panic、不靜默截斷
- 名稱會做路徑消毒，不寫出輸出目錄之外（上游直接把字串拼進 shell 命令）

## 為什麼是零依賴

只用 `std`：位置讀取用 `FileExt::read_at` / `seek_read` 讓多執行緒共用一個 `File` 不需鎖；平行池用 `std::thread::scope`；SIMD 用 `core::arch` 加執行期偵測。
