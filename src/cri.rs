//! CRI 金鑰展開與加解密（Aokana / EXTRA2 雙變體）。
//!
//! 為什麼常數參數化：兩作的容器格式完全相同，只有 `gk` 的展開常數與 `dd` 的
//! 週期／xor 常數不同。所有邏輯共用同一份程式碼，變體只換一組 `Params` 常數
//! （EXTRA2 的值來自上游 `PRead.cs`，見各欄位註解）。
//!
//! `gk` 全程 32 位元 wrapping 運算：初值 `a = seed * mul + add`，
//! `b = (a << shl) ^ a`；每輪 `a = (a - seed + b) * ((a + add2) & mask)`，
//! 其中 `b` 每輪都由本輪剛算出的 `a + add2` 重算；`out[i]` 取乘完後、右移前的
//! `a` 低位，取完才 `a >>= shr`。
//! 解密每位元組為 `((b XOR key[i % P]) + 3 + key[i % 89]) XOR C`（u8 wrapping），
//! 加密是其反運算（先 XOR C，再 wrapping 減 3 與 `key[i % 89]`，最後 XOR `key[i % P]`）。
//!
//! 為什麼以超級週期切塊：兩張表的週期（P 與 89）互質，
//! 故超級週期（兩者最小公倍數）整除兩者；每塊起點的表內容都相同，
//! 核心永遠從表索引 0 起算，不必攜帶全域偏移。

/// `gk` 展開出的金鑰流長度（位元組）。
pub const KEY_LEN: usize = 256;
/// 第二 keystream（加法用）的週期。兩變體共用。
pub const PERIOD_ADD: usize = 89;

/// .dat 的加密變體。容器格式完全相同，只差在金鑰展開與位元組轉換的常數。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// 蒼の彼方のフォーリズム（無印）
    Aokana,
    /// 蒼の彼方のフォーリズム -EXTRA2-
    Extra2,
}

impl Variant {
    /// 自動偵測時依序嘗試的順序。
    pub const ALL: [Variant; 2] = [Variant::Aokana, Variant::Extra2];

    /// header 內「檔數總和」的起始位移（Aokana 16、EXTRA2 12）；結尾一律到 1020。
    pub(crate) fn sum_start(self) -> usize {
        params(self).sum_start
    }

    /// 供錯誤訊息使用的短名稱："Aokana" / "EXTRA2"。
    pub fn name(self) -> &'static str {
        match self {
            Variant::Aokana => "Aokana",
            Variant::Extra2 => "EXTRA2",
        }
    }
}

/// 每個變體的常數。集中在一處，讓「兩者只差常數」在程式碼裡一眼可見。
struct Params {
    /// `gk` 初值 `seed * mul + add` 的乘數（Aokana 7391、EXTRA2 4892）。
    mul: u32,
    /// `gk` 初值的加數（Aokana 42828、EXTRA2 42816）。
    add: u32,
    /// `gk` 第二初值 `(a << shl) ^ a` 的移位（Aokana 17、EXTRA2 7）。
    shl: u32,
    /// `gk` 每輪 `b = a + add2` 的加數（Aokana 56、EXTRA2 156）。
    add2: u32,
    /// `gk` 每輪乘數遮罩 `b & mask`（Aokana 239、EXTRA2 206）。
    mask: u32,
    /// `gk` 每輪 `a >>= shr` 的移位（Aokana 1、EXTRA2 3）。
    shr: u32,
    /// 第一段 xor keystream 的週期（Aokana 253、EXTRA2 179）。
    period_xor: usize,
    /// `dd` 最後的 xor 常數（Aokana 153、EXTRA2 119）。
    xor_const: u8,
    /// header 內「檔數總和」的起始位移（Aokana 16、EXTRA2 12）。
    sum_start: usize,
    /// 分塊解密用的超級週期（Aokana 22517 = 253 * 89、EXTRA2 15931 = 179 * 89；
    /// 兩組週期各自互質，故最小公倍數即乘積）。
    super_period: usize,
}

// 為什麼用 const 而不是每次構造：參數在編譯期固定，呼叫端只借用靜態實例。
const AOKANA_PARAMS: Params = Params {
    mul: 7391,
    add: 42828,
    shl: 17,
    add2: 56,
    mask: 239,
    shr: 1,
    period_xor: 253,
    xor_const: 153,
    sum_start: 16,
    super_period: 22_517,
};

const EXTRA2_PARAMS: Params = Params {
    mul: 4892,
    add: 42816,
    shl: 7,
    add2: 156,
    mask: 206,
    shr: 3,
    period_xor: 179,
    xor_const: 119,
    sum_start: 12,
    super_period: 15_931,
};

fn params(variant: Variant) -> &'static Params {
    match variant {
        Variant::Aokana => &AOKANA_PARAMS,
        Variant::Extra2 => &EXTRA2_PARAMS,
    }
}

/// 由 seed 展開 256 位元組金鑰流。
///
/// 全部是 32 位元 wrapping 運算：`wrapping_shl(shl)` 等價於 C# 的 `<< shl`
/// （丟棄高位）；`>>= shr` 是 u32 的邏輯右移。
/// `b` 每輪都用本輪剛算出的 `a + add2` 重算；`out[i]` 取右移前的 `a` 低位。
pub fn gk(variant: Variant, seed: u32) -> [u8; KEY_LEN] {
    let p = params(variant);
    let mut a: u32 = seed.wrapping_mul(p.mul).wrapping_add(p.add);
    let mut b: u32 = a.wrapping_shl(p.shl) ^ a;
    let mut out = [0u8; KEY_LEN];
    for slot in out.iter_mut() {
        a = a.wrapping_sub(seed);
        a = a.wrapping_add(b);
        // 必須用本輪剛算出的 a，不能沿用上一輪的 b。
        b = a.wrapping_add(p.add2);
        a = a.wrapping_mul(b & p.mask);
        // 取右移前的 a 低位，取完才右移。
        *slot = (a & 0xFF) as u8;
        a >>= p.shr;
    }
    out
}

/// 把 `gk(seed)` 展開成兩張長度為超級週期的表。
/// 私有：呼叫端只該看到 `decrypt_in_place` / `encrypt_in_place`。
///
/// 為什麼用 `Vec` 而不用定長陣列：超級週期依變體而異（22517 或 15931）。
fn expand_tables(key: &[u8; KEY_LEN], variant: Variant) -> (Vec<u8>, Vec<u8>) {
    let p = params(variant);
    let n = p.super_period;
    let mut x = vec![0u8; n];
    let mut tab_a = vec![0u8; n];
    for (i, (xe, ae)) in x.iter_mut().zip(tab_a.iter_mut()).enumerate() {
        // i % P < P <= 253 < 256、i % 89 < 89，恆在 key 範圍內；
        // key 是內部表，非外部輸入。
        *xe = key[i % p.period_xor];
        *ae = key[i % PERIOD_ADD];
    }
    (x, tab_a)
}

/// 就地解密。
///
/// 先展開金鑰與兩張表，再把 `buf` 以超級週期切塊逐塊解密。
/// 切塊正確的前題是兩張表的週期都整除超級週期（見模組文件），
/// 因此每塊都從表的索引 0 起算。
pub fn decrypt_in_place(buf: &mut [u8], seed: u32, variant: Variant) {
    let key = gk(variant, seed);
    let (x, tab_a) = expand_tables(&key, variant);
    let p = params(variant);
    for chunk in buf.chunks_mut(p.super_period) {
        dec_kernel(chunk, &x, &tab_a, p.xor_const);
    }
}

/// 就地加密（`decrypt_in_place` 的反運算）。
pub fn encrypt_in_place(buf: &mut [u8], seed: u32, variant: Variant) {
    let key = gk(variant, seed);
    let (x, tab_a) = expand_tables(&key, variant);
    let p = params(variant);
    for chunk in buf.chunks_mut(p.super_period) {
        enc_kernel(chunk, &x, &tab_a, p.xor_const);
    }
}

/// 純量參考核心。`xor_const` 是該變體最後的 xor 常數（153 或 119）。
/// 只處理前 `buf.len()` 個位元組；`x` 與 `a` 至少要那麼長。
///
/// 每位元組做 `y = ((b XOR x) + 3 + a) XOR xor_const`，全是 u8 wrapping 運算。
/// 用 `get` 而不用索引：長度不符時只處理共同前綴，永不 panic。
pub fn decrypt_scalar(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    let mut i = 0usize;
    for b in buf.iter_mut() {
        let (Some(&xi), Some(&ai)) = (x.get(i), a.get(i)) else {
            break;
        };
        *b = ((*b ^ xi).wrapping_add(3).wrapping_add(ai)) ^ xor_const;
        i += 1;
    }
}

/// 純量參考核心。`xor_const` 是該變體最後的 xor 常數（153 或 119）。
/// 只處理前 `buf.len()` 個位元組；`x` 與 `a` 至少要那麼長。
///
/// 每位元組做 `y = ((b XOR xor_const) - 3 - a) XOR x`，是解密的反運算。
/// wrapping 減法可交換順序，故先減 3 再減 `a`。
/// 同樣只用 `get`，永不 panic。
pub fn encrypt_scalar(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    let mut i = 0usize;
    for b in buf.iter_mut() {
        let (Some(&xi), Some(&ai)) = (x.get(i), a.get(i)) else {
            break;
        };
        *b = ((*b ^ xor_const).wrapping_sub(3).wrapping_sub(ai)) ^ xi;
        i += 1;
    }
}

// 以超級週期為塊的分派核心：x86_64 走 SIMD，否則走純量。
// buf 必來自 chunks_mut(super_period)，故 buf.len() <= 表長恆成立；
// 切表時用 expect 標註此內部不變量（非外部輸入，不觸犯永不 panic 規則）。

#[cfg(target_arch = "x86_64")]
fn dec_kernel(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    if crate::cri_simd::available() {
        crate::cri_simd::dec_chunk(buf, x, a, xor_const);
    } else {
        let n = buf.len();
        let xs = x.get(..n).expect("invariant: chunk len <= super_period");
        let aa = a.get(..n).expect("invariant: chunk len <= super_period");
        decrypt_scalar(buf, xs, aa, xor_const);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn dec_kernel(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    let n = buf.len();
    let xs = x.get(..n).expect("invariant: chunk len <= super_period");
    let aa = a.get(..n).expect("invariant: chunk len <= super_period");
    decrypt_scalar(buf, xs, aa, xor_const);
}

#[cfg(target_arch = "x86_64")]
fn enc_kernel(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    if crate::cri_simd::available() {
        crate::cri_simd::enc_chunk(buf, x, a, xor_const);
    } else {
        let n = buf.len();
        let xs = x.get(..n).expect("invariant: chunk len <= super_period");
        let aa = a.get(..n).expect("invariant: chunk len <= super_period");
        encrypt_scalar(buf, xs, aa, xor_const);
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn enc_kernel(buf: &mut [u8], x: &[u8], a: &[u8], xor_const: u8) {
    let n = buf.len();
    let xs = x.get(..n).expect("invariant: chunk len <= super_period");
    let aa = a.get(..n).expect("invariant: chunk len <= super_period");
    encrypt_scalar(buf, xs, aa, xor_const);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Aokana gk(0)[0..8] = [0x00, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00]。
    // 推導要點：初值 a = 42828，b = (a << 17) ^ a；每輪 a 先減 seed 再加 b，
    // b 由本輪 a + 56 重算，out 取 (a * (b & 239)) 的低位後 a 右移一位。
    // 上值可拿紙筆從初值逐輪重算驗證（第 0 輪低位 0x98 * 192 mod 256 = 0）。
    #[test]
    fn gk_seed0_matches_hand_derivation() {
        assert_eq!(gk(Variant::Aokana, 0)[..4], [0x00, 0x80, 0x00, 0x00]);
        assert_eq!(
            gk(Variant::Aokana, 0)[..8],
            [0x00, 0x80, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00]
        );
    }

    // EXTRA2 gk(0)[0..8] = [0x00, 0xe0, 0x00, 0x40, 0x00, 0xc0, 0x00, 0x00]。
    // 來源：黃金向量檔的 `[gk] seed=0x00000000` 行（由上游 PRead.cs 的私有 gk
    // 經反射直接呼叫產生），hex 首 16 字元 `00e0004000c00000` 即前 8 位元組。
    #[test]
    fn gk_extra2_seed0_matches_upstream() {
        assert_eq!(
            gk(Variant::Extra2, 0)[..8],
            [0x00, 0xe0, 0x00, 0x40, 0x00, 0xc0, 0x00, 0x00]
        );
    }

    /// 測試字面量專用的 hex 解碼（只吃偶數長度的小寫 hex）。
    /// 為什麼不用外部 crate：零依賴；長度不符時只解共同前綴，永不 panic，
    /// 字面量寫錯會由後續的 assert_eq 抓出來。
    fn unhex(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len() / 2);
        let mut i = 0;
        while i + 1 < b.len() {
            let hi = (b[i] as char).to_digit(16).expect("test literal is hex");
            let lo = (b[i + 1] as char)
                .to_digit(16)
                .expect("test literal is hex");
            out.push(((hi << 4) | lo) as u8);
            i += 2;
        }
        out
    }

    // EXTRA2 上游黃金向量（seed 皆為 0），直接呼叫上游 PRead.cs 私有 gk/dd 產生。
    // 來源：C:\Users\YUser\_wf_tmp\w10\oracle\golden_vectors.txt 的 `dd seed=0x00000000`
    // 行（len 1 選最短、len 89/90 夾第二段週期 89 邊界、len 179/180 夾第一段週期 179 邊界）。
    // in 是密文、out 是原文：解密 in 必須得到 out，加密 out 必須得到 in。
    #[test]
    fn extra2_dd_matches_upstream_golden() {
        let cases: [(&str, &str); 5] = [
            (
                "42",
                "32",
            ),
            (
                "748afdd675eff85c6b38ca3d1f0f29af93e2ef15161f446e8845b097f76461b11eeb301b63f64ad454768741ddd4bc9e9603aea0f50f6e034c9e3112c002fa6bc2bdb580a59d82955ad5544d76b834fc07f6bc47d98197da2b",
                "003a77ae0f858c28194cba3755a55b45e192856fee553006fc3fc4ed8d1013c3569944e9118e3aa0e00e7d339760c856ee7146d48f650671381643e2b4f28a1972b74ff45fd7f22f2aaf20270eccc088fd8ec8fdab73edaa59",
            ),
            (
                "7a2c6b8be96636ffca5f11af092916b5d490677329658efec554b24c7f5c4d57457d47dd4e2c9f8d38abf4ac2449ac99073084aab647c0527f4251893ef7a0e73f7fdfe473cff732b0bc93310da5cbd4952b6f8fd9ba052225cb",
                "0ad819799b1e4e757a15e3c57bdb6e4fa0e41d01db1fe676bf20c238f528272d3ff73d9726d8d5e7ccd980d850fbd86b7d4430da4e3db422f5f2237b368dd49db5f5959001a58dc2c448e143a7dfb9a06f590525ab4a7fd25f39",
            ),
            (
                "b26f97b7fcb7d7b8b3020f2f8b563ee1b322f4e4b9fa6c2d4a566747039f56030508795915f7efd1530be31dedf450c951af0f23c5e425b86065781fd52175de8af74c97df4ab2e7b8456f9290131ae5bca2c16aaa9d6d6325b3b99d0865ed88e3e550bb69c2af42c2723b5c0fef438cb0f675282fa8a0bad1911def2e586cdd659f9cd7b1d38b3084f8218b77a5029e9cca6c8e0fed2c4bd1f9ea8950ca383c1c151bbdac57ff0143e051764938287ba560ba",
                "c205ed4d884dadcc4172e545f9ee3693c15280904b8a18c73a2e1d3db1d52e717fbc0b2b6f8d85a3e1799157878024bb23c5a551bf90dfcc141f0cd5afd30f963a8d38ed953ac29dcc3f05e22461ea9f48d2b31ada5707115f41cbd7fc1f87fc919f24491bb245323202894865a53138644e4f5ca5fc940a6363f76506ec58177ff5a8ed032199a4304c53996d9fb216e85a38a625479839c32b5abba41a4c88886fc9b798cdf5d33194230efb4cdc095f14ca",
            ),
            (
                "b911056c6f2e155b122957a176702ae7f4d06c41cc40b7be886469fc8b9742a92d99911b002d448a36405089346840c4c2dbe62c861c7707940a989752161b5a06b975fbad7d27840f2dae772700b1bc4ad774b1a9d6dcac1ffd74c35d8854c1ee2fd371a39659d0548ccbb71dde5e0b76dccba1c6e1323d66742a771cd0c06292ee703a5465598844ad2869a99c90fe3437421fb026abde27590c7dc2c09ad8a01db765744d8758a094658220c8f2f97869d4a7",
                "cba37f1805c66f29a25b2dd30e045a9d80a41833b834cd36fc101b8839ed32db472be3e974c730face3424fb401c34b0b2a99e587e680d7de0baec6d22ee692abecb0f8947f75d3065c7c60ddd7443c83aad0043dbaea85855f780b117fc20b386c5a103d1eeeba4a0f879ed577616b9ae68f9d35eb30277de80faed2864f49282a6448ae09fcb1c70075cbbbba824b640ad5215849e19963dcbb8b73214ea6cf4576d1f40c77d0cd4601ff2d4bc828b0c1ba0fd",
            ),
        ];
        for (enc_hex, dec_hex) in cases {
            let enc = unhex(enc_hex);
            let dec = unhex(dec_hex);
            let mut buf = enc.clone();
            decrypt_in_place(&mut buf, 0, Variant::Extra2);
            assert_eq!(buf, dec, "extra2 decrypt len={}", enc.len());
            let mut buf = dec.clone();
            encrypt_in_place(&mut buf, 0, Variant::Extra2);
            assert_eq!(buf, enc, "extra2 encrypt len={}", dec.len());
        }
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
    /// 因此能交叉驗證超級週期切塊邏輯。
    fn reference_decrypt(buf: &mut [u8], seed: u32, variant: Variant) {
        let p = params(variant);
        let key = gk(variant, seed);
        for (i, b) in buf.iter_mut().enumerate() {
            let xi = key[i % p.period_xor];
            let ai = key[i % PERIOD_ADD];
            *b = ((*b ^ xi).wrapping_add(3).wrapping_add(ai)) ^ p.xor_const;
        }
    }

    fn reference_encrypt(buf: &mut [u8], seed: u32, variant: Variant) {
        let p = params(variant);
        let key = gk(variant, seed);
        for (i, b) in buf.iter_mut().enumerate() {
            let xi = key[i % p.period_xor];
            let ai = key[i % PERIOD_ADD];
            *b = ((*b ^ p.xor_const).wrapping_sub(3).wrapping_sub(ai)) ^ xi;
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let seeds: [u32; 4] = [0, 89, 0x1234_5678, u32::MAX];
        for &variant in &Variant::ALL {
            // 為什麼長度表依變體組裝：超級週期不同（22517／15931），
            // 邊界值必須是各自的 sp - 1、sp、sp + 1；短長度覆蓋兩組週期邊界。
            // 為什麼直接讀欄位：Variant::super_period 方法已移除（release 要求零警告），
            // 同檔測試模組可直接取用私有的 params。
            let sp = params(variant).super_period;
            let lens: [usize; 15] = [
                0, 1, 88, 89, 90, 178, 179, 180, 252, 253, 254, sp - 1, sp, sp + 1,
                2 * sp + 1,
            ];
            for &seed in &seeds {
                for &len in &lens {
                    let mut plain = vec![0u8; len];
                    rng_fill(&mut plain, (seed as u64) ^ (len as u64).wrapping_mul(0x9E37_79B9));
                    let orig = plain.clone();

                    let mut c = plain.clone();
                    encrypt_in_place(&mut c, seed, variant);
                    decrypt_in_place(&mut c, seed, variant);
                    assert_eq!(c, orig, "enc->dec roundtrip {variant:?} seed={seed} len={len}");

                    let mut d = plain.clone();
                    decrypt_in_place(&mut d, seed, variant);
                    encrypt_in_place(&mut d, seed, variant);
                    assert_eq!(d, orig, "dec->enc roundtrip {variant:?} seed={seed} len={len}");
                }
            }
        }
    }

    /// 切塊版（`decrypt_in_place`）與全域索引參考版在各長度上逐位元組相同。
    /// 覆蓋 0..=600 與超級週期邊界（Aokana 22500..=22540、EXTRA2 15914..=15954）。
    #[test]
    fn chunked_matches_reference_on_all_lengths() {
        let seed: u32 = 0x7365_4567;
        for &variant in &Variant::ALL {
            // 為什麼直接讀欄位：Variant::super_period 方法已移除（release 要求零警告），
            // 同檔測試模組可直接取用私有的 params。
            let sp = params(variant).super_period;
            for len in (0..=600usize).chain((sp - 17)..=(sp + 23)) {
                let mut input = vec![0u8; len];
                rng_fill(&mut input, 0x1234_5678 ^ (len as u64));

                let mut via_chunks = input.clone();
                decrypt_in_place(&mut via_chunks, seed, variant);
                let mut via_ref = input.clone();
                reference_decrypt(&mut via_ref, seed, variant);
                assert_eq!(via_chunks, via_ref, "decrypt {variant:?} len={len}");

                let mut via_chunks = input.clone();
                encrypt_in_place(&mut via_chunks, seed, variant);
                let mut via_ref = input;
                reference_encrypt(&mut via_ref, seed, variant);
                assert_eq!(via_chunks, via_ref, "encrypt {variant:?} len={len}");
            }
        }
    }

    /// 交叉檢查：用 Aokana 參數解 EXTRA2 的密文，結果必須不等於原文。
    /// 若兩變體只是換名而實作相同，此測試會失敗；它證明常數真的不同。
    #[test]
    fn variants_differ_cross_check() {
        // 取黃金向量（EXTRA2 seed 0 len 90）的密文與原文。
        let enc = unhex("7a2c6b8be96636ffca5f11af092916b5d490677329658efec554b24c7f5c4d57457d47dd4e2c9f8d38abf4ac2449ac99073084aab647c0527f4251893ef7a0e73f7fdfe473cff732b0bc93310da5cbd4952b6f8fd9ba052225cb");
        let dec = unhex("0ad819799b1e4e757a15e3c57bdb6e4fa0e41d01db1fe676bf20c238f528272d3ff73d9726d8d5e7ccd980d850fbd86b7d4430da4e3db422f5f2237b368dd49db5f5959001a58dc2c448e143a7dfb9a06f590525ab4a7fd25f39");
        let mut buf = enc.clone();
        decrypt_in_place(&mut buf, 0, Variant::Aokana);
        assert_ne!(buf, dec, "aokana params must not decrypt extra2 data");
        // 反方向也一樣：EXTRA2 參數解不開 Aokana 加密的資料。
        let mut aok = dec.clone();
        encrypt_in_place(&mut aok, 0, Variant::Aokana);
        let mut buf = aok.clone();
        decrypt_in_place(&mut buf, 0, Variant::Extra2);
        assert_ne!(buf, dec, "extra2 params must not decrypt aokana data");
    }
}
