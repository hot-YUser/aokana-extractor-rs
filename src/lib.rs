//! Aokana（蒼の彼方のフォーリズム）Steam 版 .dat 拆包解密工具。
//!
//! 只做兩件事：列出 `.dat` 內容、把每個檔案解密後原樣寫出。不做任何格式轉換。
//! 零外部依賴。

pub mod cli;
pub mod cri;
pub mod dat;
pub mod error;
pub mod extract;
pub mod pathsafe;
pub mod pool;
pub mod progress;
pub mod sysio;

// cri.rs 的 SIMD 核心。刻意不公開：外部只該看到 cri::decrypt_in_place。
#[cfg(target_arch = "x86_64")]
mod cri_simd;
