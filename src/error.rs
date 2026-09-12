//! 全 crate 統一的錯誤型別。呼叫端以 `kind()` 做穩定的短標籤斷言。

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// 底層 I/O 失敗。
    Io(std::io::Error),
    /// 輸入檔不符合 .dat 格式，或結構欄位互相矛盾。
    BadFormat(String),
    /// .dat 內的檔案名稱會逃出輸出目錄，或無法安全地對應到檔案系統路徑。
    UnsafePath(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // 為什麼加前綴：呼叫端把錯誤直接印到 stderr，前綴讓使用者一眼看出是哪一層失敗。
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::BadFormat(s) => write!(f, "bad format: {s}"),
            Self::UnsafePath(s) => write!(f, "unsafe path: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // 為什麼只對 Io 回傳 source：其他變體的 String 已是完整說明，沒有更深的因果鏈。
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl Error {
    /// 穩定的短標籤，供訊息前綴與測試斷言使用：
    /// "io" / "bad-format" / "unsafe-path"
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::BadFormat(_) => "bad-format",
            Self::UnsafePath(_) => "unsafe-path",
        }
    }
}
