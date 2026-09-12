# aokana

Aokana（蒼の彼方のフォーリズム）Steam 版 `.dat` 拆包解密工具，零依賴 Rust，原生多執行緒。只做拆包解密，不做任何格式轉換。

## 建置

```
cargo build --release
```

實測 `target/release/aokana.exe` 約 195 KiB（199680 位元組）。

## 用法

```
aokana <path>                      # 拖拽模式：見下
aokana extract <path> [-o <dir>]   # 同上，可指定輸出根目錄
aokana list <file.dat>             # 列出內容
```

範例：

```
aokana data.dat
aokana extract data.dat -o ./out -j 4
aokana list data.dat
```

### 拖拽模式

把 `.dat` 拖到 `aokana.exe` 上，會在該 `.dat` 同目錄下建立同名資料夾並解到裡面
（`X.dat` → `X/`，去掉 `.dat` 副檔名）；拖一個資料夾上去則遞迴解開其中所有 `.dat`。
CLI 下 `aokana <path>` 與 `aokana extract <path> [-o <dir>]` 走完全相同的解包路徑，
`-o` 指定輸出根目錄時會鏡射來源的相對結構，避免不同子目錄下的同名 `.dat` 互撞。

進度條：解包時顯示單一進度條（含預估剩餘時間）；資料夾模式下是全部 `.dat`
共用一條，不是每個 `.dat` 一條。輸出重導向到檔案或管線（非終端機）時自動停用。
`-j N` 指定執行緒數（0 表自動），`-q` 不顯示進度與摘要，
`-h/--help` 顯示說明，`-V/--version` 顯示版本。

只有拖拽模式在失敗時會暫停等按鍵（避免總管視窗直接關閉、看不到錯誤）；
明確子命令與非終端機（管線、CI）下不暫停。

## 與上游的差異

- 不做 `adult.dat` 的 `def/version.txt` 移除（上游 `PRead.cs` 會移除；不該靜默丟檔）
- 保留名稱原始大小寫（同上游 `extract.py`）
- 所有損壞輸入回傳明確錯誤，不 panic、不靜默截斷
- 名稱會做路徑消毒，不寫出輸出目錄之外（上游直接把字串拼進 shell 命令）

## 為什麼是零依賴

只用 `std`：位置讀取用 `FileExt::read_at` / `seek_read` 讓多執行緒共用一個 `File` 不需鎖；平行池用 `std::thread::scope`；SIMD 用 `core::arch` 加執行期偵測。

## 授權

AGPL-3.0-or-later，全文見 [LICENSE](LICENSE)。
