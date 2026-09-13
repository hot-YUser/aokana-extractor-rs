//! .dat 索引解析與條目讀取。所有結構驗證失敗一律回 `Error::BadFormat`。
//!
//! 佈局（全 little-endian）：1024 位元組 header；entry 數 N 是 header 內一段
//! 區間的 LE i32 wrapping 加總（Aokana 取 `header[16..1020]` 共 251 個，
//! EXTRA2 取 `header[12..1020]` 共 252 個）；`header[212..216]` 是 entry table 金鑰，
//! `header[92..96]` 是 name table 金鑰；entry 每筆 16 位元組，欄位依序為
//! payload 長度／名稱位移／payload 金鑰／payload 位移；`entry[0].offset` 即
//! payload 區起點，name table 長度 = `data_start - 1024 - 16N`；
//! 名稱是 name table 內以 NUL 分隔的 ASCII 字串。
//!
//! `open` 依 `Variant::ALL` 順序逐一嘗試兩種變體，第一個通過全部驗證的勝出。
//! 只讀 header、entry table、name table 三個區段，不碰 payload，
//! 因此索引載入成本只與 entry 數成正比、與 payload 總量無關。

use crate::cri;
use crate::cri::Variant;
use crate::error::{Error, Result};
use crate::sysio;

/// header 固定長度（位元組）。
pub const HEADER_LEN: usize = 1024;
/// entry table 中每筆的固定長度（位元組）。
pub const ENTRY_LEN: usize = 16;

/// entry table 解密後的一筆，欄位順序為：長度／名稱位移／金鑰／位移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// payload 長度（位元組）。0 合法，表示空檔案。
    pub len: u32,
    /// 名稱在 name table 內的位移。
    pub name_off: u32,
    /// 該筆 payload 的解密金鑰。
    pub key: u32,
    /// payload 在檔案內的絕對位移。
    pub offset: u32,
}

/// 已解析並驗證過的 .dat 索引。
///
/// payload 不常駐記憶體，呼叫端以 [`DatFile::read_entry_into`] 按需讀取；
/// 底層 `File` 以 [`sysio::read_exact_at`] 做平行安全的位置讀取。
pub struct DatFile {
    file: std::fs::File,
    path: std::path::PathBuf,
    entries: Vec<Entry>,
    names: Vec<String>,
    // 偵測勝出的變體：payload 解密必須與索引解密用同一變體，否則解出來是垃圾。
    variant: Variant,
}

impl DatFile {
    /// 解析並驗證 .dat 的索引。不讀取任何 payload。
    ///
    /// 依 `Variant::ALL` 順序逐一嘗試各變體，第一個通過全部 10 條驗證規則的勝出；
    /// 全部失敗時回 `Error::BadFormat` 並附上最後一次的錯誤。
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<DatFile> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path).map_err(Error::Io)?;
        let file_len = sysio::file_len(&file)?;
        let mut last: Option<Error> = None;
        for variant in Variant::ALL {
            // File 沒有 Clone；每個變體各自開一次。失敗的變體只會多做一次
            // entry 表與 name 表的解密（只讀索引、不讀 payload），成本可忽略。
            let f = std::fs::File::open(&path).map_err(Error::Io)?;
            match parse(f, file_len, path.clone(), variant) {
                Ok(d) => return Ok(d),
                Err(e) => last = Some(e),
            }
        }
        Err(Error::BadFormat(format!(
            "unrecognized .dat: neither the Aokana nor the EXTRA2 layout validated: {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    /// 自動偵測出來的加密變體。
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// 全部條目（與 [`DatFile::names`] 一一對應）。
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 全部名稱（與 [`DatFile::entries`] 一一對應）。
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// 條目數。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否沒有任何條目（合法開啟的檔案恆為 false，因 N == 0 會被拒絕）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 底層檔案控制代碼，供平行位置讀取共用。
    pub fn file(&self) -> &std::fs::File {
        &self.file
    }

    /// 開啟時的路徑。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 第 i 筆的名稱與條目。
    ///
    /// `i` 是呼叫端的程式邏輯錯誤才會越界（不可信的檔內資料早已在 `open` 擋下，
    /// 且 entries 與 names 恆為等長），故越界時 panic 而非回傳錯誤。
    pub fn get(&self, i: usize) -> (&str, Entry) {
        let entry = self
            .entries
            .get(i)
            .expect("invariant: get() index < len(); entries and names are built in lockstep");
        let name = self
            .names
            .get(i)
            .expect("invariant: get() index < len(); entries and names are built in lockstep");
        (name.as_str(), *entry)
    }

    /// 讀取第 i 筆的 payload 並就地解密，結果放進 `buf`（會被 resize 並覆寫）。
    /// 重用同一個 `buf` 可避免反覆配置。
    ///
    /// 流程固定為：`buf.clear()` → `resize(len)` → 位置讀取 → 就地解密。
    pub fn read_entry_into(&self, i: usize, buf: &mut Vec<u8>) -> Result<()> {
        // i 越界是呼叫端錯誤；此處回 BadFormat 而非 panic，讓平行抽取能如實回報是哪一筆。
        let entry = *self.entries.get(i).ok_or_else(|| {
            Error::BadFormat(format!(
                "entry index {} out of range (len {})",
                i,
                self.entries.len()
            ))
        })?;
        buf.clear();
        let len = usize::try_from(entry.len)
            .map_err(|_| Error::BadFormat(format!("entry len too large: {}", entry.len)))?;
        buf.resize(len, 0);
        sysio::read_exact_at(&self.file, buf, entry.offset as u64)?;
        cri::decrypt_in_place(buf, entry.key, self.variant);
        Ok(())
    }
}

/// 以單一變體解析並驗證 .dat 的索引。不讀取任何 payload。
///
/// 逐項檢查檔長、entry 數、區段位移、名稱與 payload 範圍共 10 條規則，
/// 任一失敗一律回 `Error::BadFormat`。
fn parse(
    file: std::fs::File,
    file_len: u64,
    path: std::path::PathBuf,
    variant: Variant,
) -> Result<DatFile> {
    // 規則 1：檔長至少要有一個 header。
    if file_len < HEADER_LEN as u64 {
        return Err(Error::BadFormat(format!(
            "file too short for header: len {} < {}",
            file_len, HEADER_LEN
        )));
    }

    let mut header = [0u8; HEADER_LEN];
    sysio::read_exact_at(&file, &mut header, 0)?;

    // 規則 2：N = 該變體加總區間的 LE i32 wrapping 加總。
    let n = entry_count_from_header(&header, variant);

    // 規則 3：N >= 0，且 entry table 不能超出檔尾。
    if n < 0 {
        return Err(Error::BadFormat(format!("negative entry count: {}", n)));
    }
    // 規則 1 已保證 file_len >= HEADER_LEN，此減法不會下溢。
    let max_n = (file_len - HEADER_LEN as u64) / ENTRY_LEN as u64;
    if (n as u64) > max_n {
        return Err(Error::BadFormat(format!(
            "entry count {} exceeds file capacity {}",
            n, max_n
        )));
    }

    // 規則 4：N == 0 時沒有 entry[0] 可推導 name table 長度。
    if n == 0 {
        return Err(Error::BadFormat(
            "entry count is zero; cannot derive name table".to_string(),
        ));
    }

    // n > 0，故以下轉型與乘法都在已知範圍內，仍用 checked 寫法以免日後改動出錯。
    let table_len_u64 = (n as u64)
        .checked_mul(ENTRY_LEN as u64)
        .ok_or_else(|| Error::BadFormat(format!("entry table size overflows: {}", n)))?;
    let table_end = (HEADER_LEN as u64)
        .checked_add(table_len_u64)
        .ok_or_else(|| Error::BadFormat("entry table end overflows".to_string()))?;
    let table_len = usize::try_from(table_len_u64)
        .map_err(|_| Error::BadFormat(format!("entry table too large: {}", table_len_u64)))?;

    // 規則 5：讀取 entry table 並以 header[212..216] 的 u32 為金鑰解密。
    let entry_key = le_u32_at(&header, 212)?;
    let mut table = vec![0u8; table_len];
    sysio::read_exact_at(&file, &mut table, HEADER_LEN as u64)?;
    cri::decrypt_in_place(&mut table, entry_key, variant);

    let mut entries = Vec::with_capacity(n as usize);
    for chunk in table.chunks_exact(ENTRY_LEN) {
        // chunks_exact 保證每塊恰為 ENTRY_LEN 位元組。
        let len = le_u32_at(chunk, 0)?;
        let name_off = le_u32_at(chunk, 4)?;
        let key = le_u32_at(chunk, 8)?;
        let offset = le_u32_at(chunk, 12)?;
        entries.push(Entry {
            len,
            name_off,
            key,
            offset,
        });
    }

    // 規則 6：data_start 取自 entry[0].offset，且必須落在 entry table 之後、檔尾之內。
    // N > 0 已由規則 4 保證，故 entries 非空；取不到時回 BadFormat 而非 panic。
    let first = entries
        .first()
        .ok_or_else(|| Error::BadFormat("entry table is empty despite N > 0".to_string()))?;
    let data_start = first.offset as u64;
    if data_start < table_end || data_start > file_len {
        return Err(Error::BadFormat(format!(
            "data_start {} outside [{}, {}]",
            data_start, table_end, file_len
        )));
    }

    // 規則 7：讀取 name table 並以 header[92..96] 的 u32 為金鑰解密。
    // 規則 6 已保證 data_start >= table_end，此減法不會下溢。
    let name_len = data_start - table_end;
    let name_table_off = table_end;
    let name_key = le_u32_at(&header, 92)?;
    let name_buf_len = usize::try_from(name_len)
        .map_err(|_| Error::BadFormat(format!("name table too large: {}", name_len)))?;
    let mut name_table = vec![0u8; name_buf_len];
    if !name_table.is_empty() {
        sysio::read_exact_at(&file, &mut name_table, name_table_off)?;
        cri::decrypt_in_place(&mut name_table, name_key, variant);
    }

    // 規則 8：逐筆解析名稱；規則 9：逐筆檢查 payload 範圍。
    let mut names = Vec::with_capacity(entries.len());
    for e in entries.iter() {
        names.push(extract_name(&name_table, e.name_off, name_len)?);
        let payload_end = (e.offset as u64)
            .checked_add(e.len as u64)
            .ok_or_else(|| {
                Error::BadFormat(format!(
                    "payload range overflows: offset {} + len {}",
                    e.offset, e.len
                ))
            })?;
        if payload_end > file_len {
            return Err(Error::BadFormat(format!(
                "payload [{}, {}) exceeds file len {}",
                e.offset, payload_end, file_len
            )));
        }
    }

    Ok(DatFile {
        file,
        path,
        entries,
        names,
        variant,
    })
}

/// 驗證規則 2：對該變體加總區間（終點固定 1020，起點為 `variant.sum_start()`）
/// 的 little-endian i32 做 wrapping i32 加總，亦即以 u32 累加後 `as i32` 重新詮釋。
fn entry_count_from_header(header: &[u8; HEADER_LEN], variant: Variant) -> i32 {
    let start = variant.sum_start();
    let mut acc: u32 = 0;
    // 起點只可能是 12 或 16、終點恆為 1020，此範圍取值不可能失敗；用 expect 標註不變量。
    let region = header
        .get(start..1020)
        .expect("invariant: HEADER_LEN is 1024 so the variant sum range is in bounds");
    for chunk in region.chunks_exact(4) {
        // chunks_exact(4) 保證每塊恰 4 位元組。
        let mut b = [0u8; 4];
        b.copy_from_slice(chunk);
        acc = acc.wrapping_add(u32::from_le_bytes(b));
    }
    acc as i32
}

/// 從位元組串的固定位移讀一個 little-endian u32；越界時回 `Error::BadFormat`。
fn le_u32_at(buf: &[u8], off: usize) -> Result<u32> {
    let end = off
        .checked_add(4)
        .ok_or_else(|| Error::BadFormat(format!("u32 offset {} + 4 overflows", off)))?;
    let s = buf.get(off..end).ok_or_else(|| {
        Error::BadFormat(format!("u32 offset {} out of bounds (len {})", off, buf.len()))
    })?;
    let mut b = [0u8; 4];
    b.copy_from_slice(s);
    Ok(u32::from_le_bytes(b))
}

/// 驗證規則 8：從 `name_off` 起算到第一個 `0x00` 或 name table 結尾即為名稱。
///
/// - `name_off > name_len` 即 `BadFormat`。
/// - 找不到 `0x00` 時取到 table 結尾，不報錯（與上游一致）。
/// - 非 ASCII 或空名稱即 `BadFormat`。
fn extract_name(name_table: &[u8], name_off: u32, name_len: u64) -> Result<String> {
    if (name_off as u64) > name_len {
        return Err(Error::BadFormat(format!(
            "name_off {} exceeds name table len {}",
            name_off, name_len
        )));
    }
    // 上已檢查 name_off <= name_len，且 name_table.len() == name_len（usize 轉換已在 open 通過），
    // 取不到時回 BadFormat 而非 panic。
    let off = usize::try_from(name_off)
        .map_err(|_| Error::BadFormat(format!("name_off {} too large", name_off)))?;
    let rest = name_table.get(off..).ok_or_else(|| {
        Error::BadFormat(format!(
            "name_off {} out of bounds (len {})",
            name_off,
            name_table.len()
        ))
    })?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let raw = rest.get(..end).ok_or_else(|| {
        Error::BadFormat(format!(
            "name end {} out of bounds (len {})",
            end,
            rest.len()
        ))
    })?;
    if raw.is_empty() {
        return Err(Error::BadFormat(format!(
            "empty name at name_off {}",
            name_off
        )));
    }
    if !raw.is_ascii() {
        return Err(Error::BadFormat(format!(
            "non-ASCII name at name_off {}",
            name_off
        )));
    }
    // 已驗證全 ASCII，必為合法 UTF-8；失敗時仍回 BadFormat 而非 panic。
    core::str::from_utf8(raw)
        .map(str::to_owned)
        .map_err(|_| Error::BadFormat(format!("invalid UTF-8 name at name_off {}", name_off)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 寫一個暫存檔，回傳路徑；呼叫端負責斷言後刪除。
    fn write_temp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aokana-dat-unit-{}-{}.dat", std::process::id(), tag));
        std::fs::write(&p, bytes).expect("test setup: write temp file");
        p
    }

    /// 在記憶體組出最小合法 .dat：`header + entry table + name table + payloads`。
    /// entry 表與 name 表各自以對應金鑰加密；payload 各自以自己的金鑰加密。
    /// `variant` 決定加密常數與 header 加總區間；容器結構兩變體完全相同。
    fn build_dat(
        names: &[&str],
        payloads: &[Vec<u8>],
        entry_key: u32,
        name_key: u32,
        variant: Variant,
    ) -> Vec<u8> {
        assert_eq!(names.len(), payloads.len());
        let n = names.len();
        // 名稱表：NUL 分隔，逐筆記錄位移。
        let mut name_table = Vec::new();
        let mut offs: Vec<u32> = Vec::with_capacity(n);
        for nm in names {
            offs.push(name_table.len() as u32);
            name_table.extend_from_slice(nm.as_bytes());
            name_table.push(0);
        }
        let data_start = HEADER_LEN + ENTRY_LEN * n + name_table.len();
        // entry 明文表：每筆給不同的 payload 金鑰。
        let mut table = Vec::with_capacity(ENTRY_LEN * n);
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
        crate::cri::encrypt_in_place(&mut et, entry_key, variant);
        let mut nt = name_table.clone();
        crate::cri::encrypt_in_place(&mut nt, name_key, variant);
        // header：先放兩把金鑰，再回填最後一格使該變體區間的 wrapping 總和等於 N。
        // 金鑰格落在加總區間內，所以回填必須在放完金鑰之後算；
        // 最後一格 [1016..1020] 兩變體的區間都包含，故同一招對兩者都成立。
        let mut header = [0u8; HEADER_LEN];
        header[92..96].copy_from_slice(&name_key.to_le_bytes());
        header[212..216].copy_from_slice(&entry_key.to_le_bytes());
        let start = variant.sum_start();
        let count = (1020 - start) / 4;
        let mut acc: u32 = 0;
        for i in 0..count {
            let mut b = [0u8; 4];
            b.copy_from_slice(&header[start + i * 4..start + i * 4 + 4]);
            acc = acc.wrapping_add(u32::from_le_bytes(b));
        }
        let last = (n as u32).wrapping_sub(acc);
        header[1016..1020].copy_from_slice(&last.to_le_bytes());
        // payload 區：逐筆以自己的金鑰加密後拼接。
        let mut out = Vec::with_capacity(off);
        out.extend_from_slice(&header);
        out.extend_from_slice(&et);
        out.extend_from_slice(&nt);
        for (i, p) in payloads.iter().enumerate() {
            let key = 0x1234_0000u32.wrapping_add(i as u32);
            let mut enc = p.clone();
            crate::cri::encrypt_in_place(&mut enc, key, variant);
            out.extend_from_slice(&enc);
        }
        out
    }

    #[test]
    fn open_single_entry_roundtrip() {
        let payload = vec![1u8, 2, 3, 4, 5];
        let bytes = build_dat(&["a.bin"], &[payload.clone()], 0x11, 0x22, Variant::Aokana);
        let p = write_temp("single", &bytes);
        let dat = DatFile::open(&p).expect("built dat must open");
        assert_eq!(dat.variant(), Variant::Aokana);
        assert_eq!(dat.len(), 1);
        assert_eq!(dat.names(), &["a.bin".to_string()]);
        let (name, entry) = dat.get(0);
        assert_eq!(name, "a.bin");
        assert_eq!(entry.len as usize, payload.len());
        let mut buf = Vec::new();
        dat.read_entry_into(0, &mut buf)
            .expect("read entry must succeed");
        assert_eq!(buf, payload);
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn open_three_entries_with_nested_names() {
        let payloads = vec![b"hello".to_vec(), vec![0u8; 300], b"bye".to_vec()];
        let names = ["top.bin", "cg/sub/a.webp", "cg/b.webp"];
        let bytes = build_dat(&names, &payloads, 0x9E37, 0x79B9, Variant::Aokana);
        let p = write_temp("nested", &bytes);
        let dat = DatFile::open(&p).expect("built dat must open");
        assert_eq!(dat.variant(), Variant::Aokana);
        assert_eq!(dat.len(), 3);
        assert_eq!(
            dat.names(),
            &[
                "top.bin".to_string(),
                "cg/sub/a.webp".to_string(),
                "cg/b.webp".to_string()
            ]
        );
        assert_eq!(dat.entries().len(), 3);
        for (i, want) in payloads.iter().enumerate() {
            let mut buf = Vec::new();
            dat.read_entry_into(i, &mut buf)
                .expect("read entry must succeed");
            assert_eq!(&buf, want, "payload {i} mismatch");
        }
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    /// Aokana 參數組出的容器必須被偵測為 Aokana，且解出正確明文。
    /// 名稱、payload、金鑰與 EXTRA2 測試完全相同，證明偵測是唯一的差別。
    #[test]
    fn detect_aokana_variant_and_payload() {
        let payloads = vec![b"hello detector".to_vec(), vec![7u8; 500]];
        let names = ["detect/a.bin", "detect/b.bin"];
        let bytes = build_dat(&names, &payloads, 0x51, 0x7A, Variant::Aokana);
        let p = write_temp("detect-aokana", &bytes);
        let dat = DatFile::open(&p).expect("built aokana dat must open");
        assert_eq!(dat.variant(), Variant::Aokana);
        assert_eq!(dat.len(), payloads.len());
        for (i, want) in payloads.iter().enumerate() {
            let mut buf = Vec::new();
            dat.read_entry_into(i, &mut buf)
                .expect("read entry must succeed");
            assert_eq!(&buf, want, "aokana payload {i} mismatch");
        }
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    /// EXTRA2 參數組出的容器必須被偵測為 Extra2，且解出與 Aokana 測試同樣的明文。
    #[test]
    fn detect_extra2_variant_and_payload() {
        let payloads = vec![b"hello detector".to_vec(), vec![7u8; 500]];
        let names = ["detect/a.bin", "detect/b.bin"];
        let bytes = build_dat(&names, &payloads, 0x51, 0x7A, Variant::Extra2);
        let p = write_temp("detect-extra2", &bytes);
        let dat = DatFile::open(&p).expect("built extra2 dat must open");
        assert_eq!(dat.variant(), Variant::Extra2);
        assert_eq!(dat.len(), payloads.len());
        for (i, want) in payloads.iter().enumerate() {
            let mut buf = Vec::new();
            dat.read_entry_into(i, &mut buf)
                .expect("read entry must succeed");
            assert_eq!(&buf, want, "extra2 payload {i} mismatch");
        }
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn open_missing_file_is_io_error() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "aokana-dat-unit-{}-definitely-missing.dat",
            std::process::id()
        ));
        let err = DatFile::open(&p).err().expect("missing file must fail");
        assert!(matches!(err, Error::Io(_)), "expected Io, got {:?}", err);
    }

    #[test]
    fn open_short_file_is_bad_format() {
        let p = write_temp("short", &[0u8; 100]);
        let err = DatFile::open(&p).err().expect("short file must fail");
        assert!(
            matches!(err, Error::BadFormat(_)),
            "expected BadFormat, got {:?}",
            err
        );
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn open_zero_header_zero_count_is_bad_format() {
        // header 全零 → 兩變體的 N 皆為 0 → 規則 4 拒絕。
        let p = write_temp("zerocount", &[0u8; 2048]);
        let err = DatFile::open(&p).err().expect("N == 0 must fail");
        assert!(
            matches!(err, Error::BadFormat(_)),
            "expected BadFormat, got {:?}",
            err
        );
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn open_negative_count_is_bad_format() {
        // header[16..20] = 0xFFFFFFFF（即 -1），其餘為零 → wrapping 總和為 -1 → 規則 3 拒絕。
        let mut bytes = vec![0u8; 2048];
        bytes[16] = 0xFF;
        bytes[17] = 0xFF;
        bytes[18] = 0xFF;
        bytes[19] = 0xFF;
        let p = write_temp("negcount", &bytes);
        let err = DatFile::open(&p).err().expect("negative N must fail");
        assert!(
            matches!(err, Error::BadFormat(_)),
            "expected BadFormat, got {:?}",
            err
        );
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn open_all_ff_is_bad_format_not_panic() {
        // Aokana 區間 251 個 -1 的 wrapping 總和為 -251 → 規則 3 拒絕，且全程不得 panic。
        let p = write_temp("allff", &[0xFFu8; 4096]);
        let err = DatFile::open(&p).err().expect("negative N must fail");
        assert!(
            matches!(err, Error::BadFormat(_)),
            "expected BadFormat, got {:?}",
            err
        );
        assert!(std::fs::remove_file(&p).is_ok(), "cleanup temp file");
    }

    #[test]
    fn wrapping_sum_matches_spec_example() {
        // Aokana 區間 [16..1020)：251 個 1 相加得 251。
        let mut ones = [0u8; HEADER_LEN];
        for i in 0..251 {
            ones[16 + i * 4] = 1;
        }
        assert_eq!(entry_count_from_header(&ones, Variant::Aokana), 251);
        let minus = [0xFFu8; HEADER_LEN];
        // 注意 header[0..16] 與 [1020..1024] 不參與 Aokana 總和：全 FF 時總和 = 251 * -1。
        assert_eq!(entry_count_from_header(&minus, Variant::Aokana), -251);
        // EXTRA2 區間 [12..1020) 多一格：ones 在 [12..16] 為零故仍得 251；
        // 全 FF 時則為 252 * -1 = -252。
        assert_eq!(entry_count_from_header(&ones, Variant::Extra2), 251);
        assert_eq!(entry_count_from_header(&minus, Variant::Extra2), -252);
    }
}
