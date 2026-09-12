//! CRI 金鑰展開與加解密。
//!
//! `gk` 全程 32 位元 wrapping 運算：初值 `a = seed * 7391 + 42828`，
//! `b = (a << 17) ^ a`；每輪 `a = (a - seed + b) * ((a + 56) & 239)`，
//! 其中 `b` 每輪都由本輪的 `a + 56` 重算；`out[i]` 取乘完後、右移前的 `a` 低位。
//! 解密每位元組為 `((b XOR key[i % 253]) + 3 + key[i % 89]) XOR 153`（u8 wrapping），
//! 加密是其反運算（先 XOR 153，再 wrapping 減 3 與 `key[i % 89]`，最後 XOR `key[i % 253]`）。
//!
//! 為什麼用 `SUPER_PERIOD` 切塊：兩張表週期 253 與 89 互質，
//! 故 `lcm = 253 * 89 = 22517` 整除兩者；每塊起點的表內容都相同，
//! 核心永遠從表索引 0 起算，不必攜帶全域偏移。

/// `gk` 展開出的金鑰流長度（位元組）。
pub const KEY_LEN: usize = 256;
/// 第一 keystream（xor 用）的週期。
pub const PERIOD_XOR: usize = 253;
/// 第二 keystream（加法用）的週期。
pub const PERIOD_ADD: usize = 89;
/// lcm(253, 89)。253 = 11 * 23，與質數 89 互質，故 lcm 即乘積 253 * 89。
pub const SUPER_PERIOD: usize = 22_517;

/// 由 seed 展開 256 位元組金鑰流。
///
/// 全部是 32 位元 wrapping 運算：`wrapping_shl(17)` 等價於 C# 的 `<< 17`
/// （丟棄高位）；`>> 1` 是 u32 的邏輯右移。
/// `b` 每輪都用本輪剛算出的 `a + 56` 重算；`out[i]` 取右移前的 `a` 低位。
pub fn gk(seed: u32) -> [u8; KEY_LEN] {
    let mut a: u32 = seed.wrapping_mul(7391).wrapping_add(42828);
    let mut b: u32 = a.wrapping_shl(17) ^ a;
    let mut out = [0u8; KEY_LEN];
    for slot in out.iter_mut() {
        a = a.wrapping_sub(seed);
        a = a.wrapping_add(b);
        // 必須用本輪剛算出的 a，不能沿用上一輪的 b。
        b = a.wrapping_add(56);
        a = a.wrapping_mul(b & 239);
        // 取右移前的 a 低位，取完才右移。
        *slot = (a & 0xFF) as u8;
        a >>= 1;
    }
    out
}

/// 把 `gk(seed)` 展開成兩張長度為 `SUPER_PERIOD` 的表。
/// 私有：呼叫端只該看到 `decrypt_in_place` / `encrypt_in_place`。
fn expand_tables(key: &[u8; KEY_LEN]) -> ([u8; SUPER_PERIOD], [u8; SUPER_PERIOD]) {
    let mut x = [0u8; SUPER_PERIOD];
    let mut tab_a = [0u8; SUPER_PERIOD];
    for (i, (xe, ae)) in x.iter_mut().zip(tab_a.iter_mut()).enumerate() {
        // i % 253 < 253、i % 89 < 89，恆在 key 範圍內；key 是內部表，非外部輸入。
        *xe = key[i % PERIOD_XOR];
        *ae = key[i % PERIOD_ADD];
    }
    (x, tab_a)
}

/// 就地解密。
///
/// 先展開金鑰與兩張表，再把 `buf` 以 `SUPER_PERIOD` 切塊逐塊解密。
/// 切塊正確的前題是兩張表的週期都整除 `SUPER_PERIOD`（見模組文件），
/// 因此每塊都從表的索引 0 起算。
pub fn decrypt_in_place(buf: &mut [u8], seed: u32) {
    let key = gk(seed);
    let (x, tab_a) = expand_tables(&key);
    for chunk in buf.chunks_mut(SUPER_PERIOD) {
        dec_kernel(chunk, &x, &tab_a);
    }
}

/// 就地加密（`decrypt_in_place` 的反運算）。
pub fn encrypt_in_place(buf: &mut [u8], seed: u32) {
    let key = gk(seed);
    let (x, tab_a) = expand_tables(&key);
    for chunk in buf.chunks_mut(SUPER_PERIOD) {
        enc_kernel(chunk, &x, &tab_a);
    }
}

/// 純量參考核心，供測試與非 x86_64 平台使用。
/// `x.len() == a.len()`，且只處理前 `buf.len()` 個位元組。
///
/// 每位元組做 `y = ((b XOR x) + 3 + a) XOR 153`，全是 u8 wrapping 運算。
/// 用 `get` 而不用索引：長度不符時只處理共同前綴，永不 panic。
pub fn decrypt_scalar(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let mut i = 0usize;
    for b in buf.iter_mut() {
        let (Some(&xi), Some(&ai)) = (x.get(i), a.get(i)) else {
            break;
        };
        *b = ((*b ^ xi).wrapping_add(3).wrapping_add(ai)) ^ 153;
        i += 1;
    }
}

/// 純量參考核心，供測試與非 x86_64 平台使用。
/// `x.len() == a.len()`，且只處理前 `buf.len()` 個位元組。
///
/// 每位元組做 `y = ((b XOR 153) - 3 - a) XOR x`，是解密的反運算。
/// wrapping 減法可交換順序，故先減 3 再減 `a`。
/// 同樣只用 `get`，永不 panic。
pub fn encrypt_scalar(buf: &mut [u8], x: &[u8], a: &[u8]) {
    let mut i = 0usize;
    for b in buf.iter_mut() {
        let (Some(&xi), Some(&ai)) = (x.get(i), a.get(i)) else {
            break;
        };
        *b = ((*b ^ 153).wrapping_sub(3).wrapping_sub(ai)) ^ xi;
        i += 1;
    }
}

// 以 SUPER_PERIOD 為塊的分派核心：x86_64 走 SIMD，否則走純量。
// buf 必來自 chunks_mut(SUPER_PERIOD)，故 buf.len() <= SUPER_PERIOD 恆成立；
// 切表時用 expect 標註此內部不變量（非外部輸入，不觸犯永不 panic 規則）。

#[cfg(target_arch = "x86_64")]
fn dec_kernel(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    if crate::cri_simd::available() {
        crate::cri_simd::dec_chunk(buf, x, a);
    } else {
        let n = buf.len();
        let xs = x.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
        let aa = a.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
        decrypt_scalar(buf, xs, aa);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn dec_kernel(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    let n = buf.len();
    let xs = x.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
    let aa = a.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
    decrypt_scalar(buf, xs, aa);
}

#[cfg(target_arch = "x86_64")]
fn enc_kernel(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    if crate::cri_simd::available() {
        crate::cri_simd::enc_chunk(buf, x, a);
    } else {
        let n = buf.len();
        let xs = x.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
        let aa = a.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
        encrypt_scalar(buf, xs, aa);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn enc_kernel(buf: &mut [u8], x: &[u8; SUPER_PERIOD], a: &[u8; SUPER_PERIOD]) {
    let n = buf.len();
    let xs = x.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
    let aa = a.get(..n).expect("invariant: chunk len <= SUPER_PERIOD");
    encrypt_scalar(buf, xs, aa);
}

#[cfg(test)]
mod tests {
    use super::*;

    // gk(0)[0..8] = [0x00, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00]。
    // 推導要點：初值 a = 42828，b = (a << 17) ^ a；每輪 a 先減 seed 再加 b，
    // b 由本輪 a + 56 重算，out 取 (a * (b & 239)) 的低位後 a 右移一位。
    // 上值可拿紙筆從初值逐輪重算驗證（第 0 輪低位 0x98 * 192 mod 256 = 0）。
    #[test]
    fn gk_seed0_matches_hand_derivation() {
        assert_eq!(gk(0)[..4], [0x00, 0x80, 0x00, 0x00]);
        assert_eq!(gk(0)[..8], [0x00, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00]);
    }

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

    /// 不經切塊的全域索引純量實作：與切塊版逐位元組相同才算對。
    /// 它只用一張 key 表、以下標取全域 i，是獨立於 expand_tables 的寫法，
    /// 因此能交叉驗證 SUPER_PERIOD 切塊邏輯。
    fn reference_decrypt(buf: &mut [u8], seed: u32) {
        let key = gk(seed);
        for (i, b) in buf.iter_mut().enumerate() {
            let xi = key[i % PERIOD_XOR];
            let ai = key[i % PERIOD_ADD];
            *b = ((*b ^ xi).wrapping_add(3).wrapping_add(ai)) ^ 153;
        }
    }

    fn reference_encrypt(buf: &mut [u8], seed: u32) {
        let key = gk(seed);
        for (i, b) in buf.iter_mut().enumerate() {
            let xi = key[i % PERIOD_XOR];
            let ai = key[i % PERIOD_ADD];
            *b = ((*b ^ 153).wrapping_sub(3).wrapping_sub(ai)) ^ xi;
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let seeds: [u32; 4] = [0, 89, 0x1234_5678, u32::MAX];
        let lens: [usize; 12] = [
            0, 1, 88, 89, 90, 252, 253, 254, 22_516, 22_517, 22_518, 45_035,
        ];
        for &seed in &seeds {
            for &len in &lens {
                let mut plain = vec![0u8; len];
                rng_fill(&mut plain, (seed as u64) ^ (len as u64).wrapping_mul(0x9E37_79B9));
                let orig = plain.clone();

                let mut c = plain.clone();
                encrypt_in_place(&mut c, seed);
                decrypt_in_place(&mut c, seed);
                assert_eq!(c, orig, "enc->dec roundtrip seed={seed} len={len}");

                let mut d = plain.clone();
                decrypt_in_place(&mut d, seed);
                encrypt_in_place(&mut d, seed);
                assert_eq!(d, orig, "dec->enc roundtrip seed={seed} len={len}");
            }
        }
    }

    /// 切塊版（`decrypt_in_place`）與全域索引參考版在各長度上逐位元組相同。
    /// 覆蓋 0..=600 與 22500..=22540（後者橫跨 SUPER_PERIOD = 22517 邊界）。
    #[test]
    fn chunked_matches_reference_on_all_lengths() {
        let seed: u32 = 0x7365_4567;
        for len in (0..=600usize).chain(22_500..=22_540) {
            let mut input = vec![0u8; len];
            rng_fill(&mut input, 0x1234_5678 ^ (len as u64));

            let mut via_chunks = input.clone();
            decrypt_in_place(&mut via_chunks, seed);
            let mut via_ref = input.clone();
            reference_decrypt(&mut via_ref, seed);
            assert_eq!(via_chunks, via_ref, "decrypt len={len}");

            let mut via_chunks = input.clone();
            encrypt_in_place(&mut via_chunks, seed);
            let mut via_ref = input;
            reference_encrypt(&mut via_ref, seed);
            assert_eq!(via_chunks, via_ref, "encrypt len={len}");
        }
    }
}
