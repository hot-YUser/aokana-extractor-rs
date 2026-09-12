//! 名稱轉安全相對路徑：.dat 內的名稱不可信，必須先擋下逃逸與非法元件。
//!
//! 上游直接把字串拼進 shell 命令，有路徑跳脫與注入風險；
//! 此處把名稱拆成元件逐一檢查，通過後才交給呼叫端與輸出目錄拼接。

use crate::error::{Error, Result};

/// 把 .dat 內的名稱轉成安全的相對路徑。
/// 拒絕（回傳 Error::UnsafePath）：
///   - 空名稱
///   - 任何空元件（連續分隔符、開頭或結尾的分隔符）
///   - 元件為 "." 或 ".."
///   - 任何元件含 ':' 或 '\0'
///   - 以 '/' 或 '\\' 開頭（絕對路徑）
/// 分隔符同時接受 '/' 與 '\\'。回傳的路徑以 '/' 的等價形式（std::path 元件）表示。
pub fn safe_relative_path(name: &str) -> Result<std::path::PathBuf> {
    // components 已做全部檢查；此處逐元件 push，不做任何字串拼接，
    // 因此結果恆為相對路徑，不可能憑空長出絕對路徑或父目錄參照。
    let mut out = std::path::PathBuf::new();
    for c in components(name)?.iter() {
        out.push(c);
    }
    Ok(out)
}

/// 同上，但不建立 PathBuf，只回傳切成元件的迭代結果，供顯示用。
pub fn components(name: &str) -> Result<Vec<&str>> {
    if name.is_empty() {
        return Err(Error::UnsafePath("empty name".to_string()));
    }
    // 以 '/' 或 '\\' 開頭即絕對路徑；此檢查在空元件檢查之前，讓錯誤訊息更明確。
    // （開頭分隔符本來也會產生空首元件，兩層檢查是一致的。）
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(Error::UnsafePath(format!(
            "absolute path not allowed: {:?}",
            name
        )));
    }
    // 以分隔符切分並保留空元件：連續、開頭、結尾的分隔符都會留下空字串，
    // 下面的迴圈會把它們全數拒絕。
    let parts: Vec<&str> = name.split(|c| c == '/' || c == '\\').collect();
    for p in parts.iter() {
        if p.is_empty() {
            return Err(Error::UnsafePath(format!(
                "empty component in name: {:?}",
                name
            )));
        }
        // "." 留在原地無意義，".." 則會逃出輸出目錄。
        if *p == "." || *p == ".." {
            return Err(Error::UnsafePath(format!(
                "dot component {:?} in name: {:?}",
                p, name
            )));
        }
        // ':' 在 Windows 是磁碟機與 ADS 語法，一律拒絕以保跨平台安全；
        // '\0' 會截斷底層 C API 的路徑字串，絕不能放行。
        if p.contains(':') || p.contains('\0') {
            return Err(Error::UnsafePath(format!(
                "illegal character in component {:?} of name: {:?}",
                p, name
            )));
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_names_pass() {
        assert_eq!(components("a").expect("single"), vec!["a"]);
        assert_eq!(components("a/b").expect("slash"), vec!["a", "b"]);
        assert_eq!(
            components("dir/sub/file.txt").expect("nested"),
            vec!["dir", "sub", "file.txt"]
        );
        // 反斜線是合法分隔符，與 '/' 等價。
        assert_eq!(components("a\\b").expect("backslash"), vec!["a", "b"]);
        assert_eq!(
            components("a\\b/c").expect("mixed"),
            vec!["a", "b", "c"]
        );
        // 點出現在元件內部（非整個元件）是合法的。
        assert_eq!(
            components("a.b/c..d/.../e").expect("dots inside"),
            vec!["a.b", "c..d", "...", "e"]
        );
    }

    #[test]
    fn safe_relative_path_joins_components() {
        let p = safe_relative_path("a\\b").expect("a\\b is safe");
        assert_eq!(p, std::path::Path::new("a").join("b"));
        let q = safe_relative_path("x/y/z.txt").expect("nested is safe");
        assert_eq!(
            q,
            std::path::Path::new("x")
                .join("y")
                .join("z.txt")
        );
    }

    #[test]
    fn malicious_names_rejected() {
        for bad in [
            "../x",       // 父目錄逃逸
            "a/../../b",  // 中段逃逸
            "C:/x",       // 磁碟機語法（含 ':'）
            "a//b",       // 空元件
            "",           // 空名稱
            "/abs",       // 絕對路徑
            "\\abs",      // 反斜線絕對路徑
            "a/",         // 結尾分隔符
            "a\\",        // 結尾反斜線
            ".",          // 當前目錄
            "..",         // 父目錄
            "a/./b",      // 當前目錄元件
            "a/../b",     // 父目錄元件
            "a:b",        // 元件含 ':'
            "a/b:c",      // 後段元件含 ':'
            "a\0b",       // 元件含 NUL
        ] {
            let err = components(bad).expect_err(&format!("{:?} must be rejected", bad));
            assert!(
                matches!(err, Error::UnsafePath(_)),
                "wrong variant for {:?}: {:?}",
                bad,
                err
            );
            let err2 =
                safe_relative_path(bad).expect_err(&format!("{:?} must be rejected", bad));
            assert!(
                matches!(err2, Error::UnsafePath(_)),
                "wrong variant for {:?}: {:?}",
                bad,
                err2
            );
        }
    }
}
