//! x86_64 AVX2 / SSE2 加解密核心與執行期分派。全 crate 唯一允許 `unsafe` 的檔案。
//!
//! 為什麼分三條路徑：AVX2 一次 32 位元組，SSE2 一次 16 位元組（SSE2 是 x86_64
//! 基準指令集，不必偵測），不足一個向量的尾端走明確的純量迴圈。
//! 解密與加密的每位元組運算都沒有跨通道相依，可以直接逐位元組向量化；
//! 向量通道內的加減本來就是 wrapping，與純量的 `wrapping_add`／`wrapping_sub`
//! 語意一致。位元組語意的權威定義見同 crate 的 `src/cri.rs` 模組註解：
//! 解密為 `((b XOR x) + 3 + a) XOR 153`，加密為其反運算 `((b XOR 153) - 3 - a) XOR x`。
//! 呼叫契約：`buf.len() <= cri::SUPER_PERIOD`；`x` 與 `a` 是長度為 `SUPER_PERIOD`
//! 的兩張表，索引自 0 起算。

use core::arch::x86_64::{
    __m128i, __m256i, _mm_add_epi8, _mm_loadu_si128, _mm_set1_epi8, _mm_storeu_si128,
    _mm_sub_epi8, _mm_xor_si128, _mm256_add_epi8, _mm256_loadu_si256, _mm256_set1_epi8,
    _mm256_storeu_si256, _mm256_sub_epi8, _mm256_xor_si256,
};

use crate::cri::SUPER_PERIOD;

/// 本機 CPU 是否支援 AVX2 路徑（以 OnceLock 快取偵測結果）。
///
/// 為什麼快取：`is_x86_feature_detected!` 每次呼叫都有成本，而加解密是熱路徑；
/// 偵測結果在行程內不會改變，所以只測一次。
pub fn available() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

/// 就地解密前 `buf.len()` 個位元組，索引自 0 起算。
/// `buf.len() <= cri::SUPER_PERIOD` 由呼叫端保證。
///
/// 為什麼只處理前 `min(len, SUPER_PERIOD)`：呼叫端守約時等於全長；
/// 若契約被違反，寧可少做也不讀出表外（永不 panic、永不越界）。
pub fn dec_chunk(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    let n = buf.len().min(SUPER_PERIOD);
    if n == 0 {
        return;
    }
    // n <= SUPER_PERIOD == x.len() == a.len() 且 n <= buf.len() 恆成立；
    // 取不到只可能是內部算錯，直接返回（不做任何事是最安全的退化）。
    let (Some(xs), Some(aa), Some(prefix)) = (x.get(..n), a.get(..n), buf.get_mut(..n)) else {
        return;
    };
    if available() {
        // SAFETY: 已用 available()（即 is_x86_feature_detected!("avx2") 的快取值）
        // 確認本機支援 AVX2，滿足 dec_avx2 的 target_feature 契約；
        // 三個切片等長為 n，dec_avx2 只讀寫 [0, n)，不越界。
        unsafe {
            dec_avx2(prefix, xs, aa);
        }
    } else {
        dec_sse2(prefix, xs, aa);
    }
}

/// 就地加密前 `buf.len()` 個位元組，索引自 0 起算。
/// `buf.len() <= cri::SUPER_PERIOD` 由呼叫端保證。
///
/// 長度退化策略同 `dec_chunk`：只處理前 `min(len, SUPER_PERIOD)`。
pub fn enc_chunk(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    let n = buf.len().min(SUPER_PERIOD);
    if n == 0 {
        return;
    }
    let (Some(xs), Some(aa), Some(prefix)) = (x.get(..n), a.get(..n), buf.get_mut(..n)) else {
        return;
    };
    if available() {
        // SAFETY: 同 dec_chunk，available() 已保證 AVX2 可用；
        // 三個切片等長為 n，enc_avx2 只讀寫 [0, n)，不越界。
        unsafe {
            enc_avx2(prefix, xs, aa);
        }
    } else {
        enc_sse2(prefix, xs, aa);
    }
}

// AVX2 解密主體：32 位元組／迭代，不足部分走純量尾端。
//
// 呼叫端必須已用 `available()` 確認本機支援 AVX2（見 dec_chunk）。
// SAFETY（函式級）：呼叫端保證 AVX2 可用；buf、x、a 的有效長度取三者最小值 n，
// 函式只讀寫各自的 [0, n)，不越界、不觸及呼叫端的其他記憶體。
#[target_feature(enable = "avx2")]
unsafe fn dec_avx2(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let n = buf.len().min(x.len()).min(a.len());
    if n == 0 {
        return;
    }
    // 註：廣播常數只寫入向量暫存器，不觸及記憶體；
    // 位元模式 0x03／0x99 與純量語意的 wrapping 加數／xor 常數一致。
    let (v3, v153): (__m256i, __m256i) = (
        _mm256_set1_epi8(3),
        _mm256_set1_epi8(153u8 as i8),
    );
    let bp = buf.as_mut_ptr();
    let xp = x.as_ptr();
    let ap = a.as_ptr();
    // 向量主迴圈處理 [0, vec_end)，vec_end = (n / 32) * 32。
    let vec_end = (n / 32) * 32;
    // SAFETY: vec_end <= n <= 三個切片長度，每次迭代讀寫 [off, off + 32) 全在界內；
    // loadu/storeu 容許非對齊位址；三個指標源自互不重疊的切片，不存在別名寫入；
    // off < vec_end <= 切片長度 <= isize::MAX，off += 32 不溢位。
    unsafe {
        let mut off: usize = 0;
        while off < vec_end {
            let b = _mm256_loadu_si256(bp.add(off) as *const __m256i);
            let xv = _mm256_loadu_si256(xp.add(off) as *const __m256i);
            let av = _mm256_loadu_si256(ap.add(off) as *const __m256i);
            // y = ((b XOR x) + 3 + a) XOR 153，全是逐位元組運算。
            let mut t = _mm256_xor_si256(b, xv);
            t = _mm256_add_epi8(t, v3);
            t = _mm256_add_epi8(t, av);
            t = _mm256_xor_si256(t, v153);
            _mm256_storeu_si256(bp.add(off) as *mut __m256i, t);
            off += 32;
        }
    }
    // SAFETY: vec_end <= i < n <= 三個切片長度，裸指標讀寫皆在界內；
    // 逐位元組 wrapping 語意與純量參考實作逐位元組一致；索引從 vec_end 接續，
    // 不與主迴圈重疊（不可對整個緩衝區再跑一次純量）。
    unsafe {
        let mut i = vec_end;
        while i < n {
            let b = *bp.add(i);
            let xi = *xp.add(i);
            let ai = *ap.add(i);
            *bp.add(i) = (b ^ xi).wrapping_add(3).wrapping_add(ai) ^ 153;
            i += 1;
        }
    }
}

// AVX2 加密主體：解密的反運算，32 位元組／迭代。
//
// 呼叫端必須已用 `available()` 確認本機支援 AVX2（見 enc_chunk）。
// SAFETY（函式級）：同 dec_avx2；只讀寫各自的 [0, n)。
#[target_feature(enable = "avx2")]
unsafe fn enc_avx2(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let n = buf.len().min(x.len()).min(a.len());
    if n == 0 {
        return;
    }
    // 註：同 dec_avx2，廣播常數只進暫存器，不觸及記憶體。
    let (v3, v153): (__m256i, __m256i) = (
        _mm256_set1_epi8(3),
        _mm256_set1_epi8(153u8 as i8),
    );
    let bp = buf.as_mut_ptr();
    let xp = x.as_ptr();
    let ap = a.as_ptr();
    let vec_end = (n / 32) * 32;
    // SAFETY: 同 dec_avx2 主迴圈；每次迭代讀寫 [off, off + 32) 全在界內。
    unsafe {
        let mut off: usize = 0;
        while off < vec_end {
            let b = _mm256_loadu_si256(bp.add(off) as *const __m256i);
            let xv = _mm256_loadu_si256(xp.add(off) as *const __m256i);
            let av = _mm256_loadu_si256(ap.add(off) as *const __m256i);
            // y = ((b XOR 153) - 3 - a) XOR x；u8 減法 wrapping，正好是解密的反運算。
            let mut t = _mm256_xor_si256(b, v153);
            t = _mm256_sub_epi8(t, v3);
            t = _mm256_sub_epi8(t, av);
            t = _mm256_xor_si256(t, xv);
            _mm256_storeu_si256(bp.add(off) as *mut __m256i, t);
            off += 32;
        }
    }
    // SAFETY: 同 dec_avx2 尾端；索引從 vec_end 接續，不與主迴圈重疊。
    unsafe {
        let mut i = vec_end;
        while i < n {
            let b = *bp.add(i);
            let xi = *xp.add(i);
            let ai = *ap.add(i);
            *bp.add(i) = (b ^ 153).wrapping_sub(3).wrapping_sub(ai) ^ xi;
            i += 1;
        }
    }
}

// SSE2 解密主體：16 位元組／迭代，不足部分走純量尾端。
//
// 為什麼不需要 target_feature 也不需要偵測：SSE2 是 x86_64 的基準指令集，
// 任何 x86_64 CPU 都支援。
fn dec_sse2(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let n = buf.len().min(x.len()).min(a.len());
    if n == 0 {
        return;
    }
    // SAFETY: 廣播常數只寫入向量暫存器，不觸及記憶體。
    let (v3, v153): (__m128i, __m128i) = unsafe {
        (_mm_set1_epi8(3), _mm_set1_epi8(153u8 as i8))
    };
    let bp = buf.as_mut_ptr();
    let xp = x.as_ptr();
    let ap = a.as_ptr();
    let vec_end = (n / 16) * 16;
    // SAFETY: vec_end <= n <= 三個切片長度，每次迭代讀寫 [off, off + 16) 全在界內；
    // loadu/storeu 容許非對齊；三個指標源自互不重疊的切片；
    // off < vec_end <= 切片長度，off += 16 不溢位。
    unsafe {
        let mut off: usize = 0;
        while off < vec_end {
            let b = _mm_loadu_si128(bp.add(off) as *const __m128i);
            let xv = _mm_loadu_si128(xp.add(off) as *const __m128i);
            let av = _mm_loadu_si128(ap.add(off) as *const __m128i);
            // y = ((b XOR x) + 3 + a) XOR 153，全是逐位元組運算。
            let mut t = _mm_xor_si128(b, xv);
            t = _mm_add_epi8(t, v3);
            t = _mm_add_epi8(t, av);
            t = _mm_xor_si128(t, v153);
            _mm_storeu_si128(bp.add(off) as *mut __m128i, t);
            off += 16;
        }
    }
    // SAFETY: vec_end <= i < n <= 三個切片長度，裸指標讀寫皆在界內；
    // 索引從 vec_end 接續，不與主迴圈重疊。
    unsafe {
        let mut i = vec_end;
        while i < n {
            let b = *bp.add(i);
            let xi = *xp.add(i);
            let ai = *ap.add(i);
            *bp.add(i) = (b ^ xi).wrapping_add(3).wrapping_add(ai) ^ 153;
            i += 1;
        }
    }
}

// SSE2 加密主體：解密的反運算，16 位元組／迭代。
//
// 為什麼不需要偵測：同 dec_sse2，SSE2 是 x86_64 基準。
fn enc_sse2(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let n = buf.len().min(x.len()).min(a.len());
    if n == 0 {
        return;
    }
    // SAFETY: 同 dec_sse2，廣播常數只進暫存器。
    let (v3, v153): (__m128i, __m128i) = unsafe {
        (_mm_set1_epi8(3), _mm_set1_epi8(153u8 as i8))
    };
    let bp = buf.as_mut_ptr();
    let xp = x.as_ptr();
    let ap = a.as_ptr();
    let vec_end = (n / 16) * 16;
    // SAFETY: 同 dec_sse2 主迴圈；每次迭代讀寫 [off, off + 16) 全在界內。
    unsafe {
        let mut off: usize = 0;
        while off < vec_end {
            let b = _mm_loadu_si128(bp.add(off) as *const __m128i);
            let xv = _mm_loadu_si128(xp.add(off) as *const __m128i);
            let av = _mm_loadu_si128(ap.add(off) as *const __m128i);
            // y = ((b XOR 153) - 3 - a) XOR x。
            let mut t = _mm_xor_si128(b, v153);
            t = _mm_sub_epi8(t, v3);
            t = _mm_sub_epi8(t, av);
            t = _mm_xor_si128(t, xv);
            _mm_storeu_si128(bp.add(off) as *mut __m128i, t);
            off += 16;
        }
    }
    // SAFETY: 同 dec_sse2 尾端；索引從 vec_end 接續，不與主迴圈重疊。
    unsafe {
        let mut i = vec_end;
        while i < n {
            let b = *bp.add(i);
            let xi = *xp.add(i);
            let ai = *ap.add(i);
            *bp.add(i) = (b ^ 153).wrapping_sub(3).wrapping_sub(ai) ^ xi;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 無依賴的確定性 PRNG（xorshift64*），只供測試填充資料。
    fn rng_fill(buf: &mut [u8], mut s: u64) {
        if s == 0 {
            s = 0x9E37_79B9_7F4A_7C15;
        }
        for b in buf.iter_mut() {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            *b = (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8;
        }
    }

    /// 測試內手寫的純量解密（獨立於 `cri::decrypt_scalar` 的第二實作）。
    /// 只用 `get`／`get_mut`，任何長度組合都不 panic。
    fn scalar_dec(buf: &mut [u8], x: &[u8], a: &[u8]) {
        let n = buf.len().min(x.len()).min(a.len());
        let mut i = 0;
        while i < n {
            if let (Some(b), Some(&xi), Some(&ai)) = (buf.get_mut(i), x.get(i), a.get(i)) {
                *b = (*b ^ xi).wrapping_add(3).wrapping_add(ai) ^ 153;
            }
            i += 1;
        }
    }

    /// 測試內手寫的純量加密（獨立於 `cri::encrypt_scalar` 的第二實作）。
    fn scalar_enc(buf: &mut [u8], x: &[u8], a: &[u8]) {
        let n = buf.len().min(x.len()).min(a.len());
        let mut i = 0;
        while i < n {
            if let (Some(b), Some(&xi), Some(&ai)) = (buf.get_mut(i), x.get(i), a.get(i)) {
                *b = (*b ^ 153).wrapping_sub(3).wrapping_sub(ai) ^ xi;
            }
            i += 1;
        }
    }

    /// AVX2、SSE2 與手寫純量在各長度上輸出完全相同。
    /// 覆蓋 0..=600 與 22500..=22517（後者橫跨 SUPER_PERIOD = 22517 邊界）。
    /// 為什麼不窮盡：窮盡 22518 個長度對小工具太重；向量主迴圈與純量尾端的
    /// 接縫（32 與 16 位元組步進的每個餘數）已由 0..=600 的每個餘數覆蓋，
    /// 再保留一個跨 SUPER_PERIOD 邊界的長度（22517）即可。
    /// 只能到 SUPER_PERIOD：上層 `dec_chunk`/`enc_chunk` 的凍結契約要求
    /// `buf.len() <= SUPER_PERIOD`，超過的部分由 `cri.rs` 的切塊測試覆蓋，
    /// 因此此測試用等長的前綴切片當表，而不用定長的 SUPER_PERIOD 陣列。
    /// 為什麼在迴圈外一次配置：每長度重配緩衝區會疊出 O(N^2) 的配置成本；
    /// 共用整長緩衝區、迴圈內只取前綴切片。
    #[test]
    fn avx2_sse2_scalar_agree_on_all_lengths() {
        if !available() {
            return;
        }
        let mut buf = vec![0u8; SUPER_PERIOD];
        let mut xs = vec![0u8; SUPER_PERIOD];
        let mut aa = vec![0u8; SUPER_PERIOD];
        rng_fill(&mut buf, 0x9E37_79B9_7F4A_7C15 ^ 0x11);
        rng_fill(&mut xs, 0x9E37_79B9_7F4A_7C15 ^ 0x22);
        rng_fill(&mut aa, 0x9E37_79B9_7F4A_7C15 ^ 0x33);
        let mut v_avx2 = vec![0u8; SUPER_PERIOD];
        let mut v_sse2 = vec![0u8; SUPER_PERIOD];
        let mut v_ref = vec![0u8; SUPER_PERIOD];
        for len in (0..=600usize).chain(22_500..=SUPER_PERIOD) {
            let src = buf.get(..len).expect("invariant: len <= SUPER_PERIOD");
            let xt = xs.get(..len).expect("invariant: len <= SUPER_PERIOD");
            let at = aa.get(..len).expect("invariant: len <= SUPER_PERIOD");

            let dst = v_avx2.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            // SAFETY: 本測試開頭已確認 available() 為真，滿足 dec_avx2 的
            // target_feature 契約；三個切片等長為 len，只讀寫界內。
            unsafe {
                dec_avx2(dst, xt, at);
            }
            let dst = v_sse2.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            dec_sse2(dst, xt, at);
            let dst = v_ref.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            scalar_dec(dst, xt, at);
            assert_eq!(
                v_avx2.get(..len).expect("invariant"),
                v_sse2.get(..len).expect("invariant"),
                "dec avx2 vs sse2 len={len}"
            );
            assert_eq!(
                v_avx2.get(..len).expect("invariant"),
                v_ref.get(..len).expect("invariant"),
                "dec simd vs scalar len={len}"
            );

            let dst = v_avx2.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            // SAFETY: 同上，available() 已為真；三個切片等長為 len。
            unsafe {
                enc_avx2(dst, xt, at);
            }
            let dst = v_sse2.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            enc_sse2(dst, xt, at);
            let dst = v_ref.get_mut(..len).expect("invariant: len <= SUPER_PERIOD");
            dst.copy_from_slice(src);
            scalar_enc(dst, xt, at);
            assert_eq!(
                v_avx2.get(..len).expect("invariant"),
                v_sse2.get(..len).expect("invariant"),
                "enc avx2 vs sse2 len={len}"
            );
            assert_eq!(
                v_avx2.get(..len).expect("invariant"),
                v_ref.get(..len).expect("invariant"),
                "enc simd vs scalar len={len}"
            );
        }
    }
}
