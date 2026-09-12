//! 以 `std::thread::scope` 實作的索引範圍平行池，動態分配以平衡大小不一的工作。

use crate::error::Result;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 對 0..n 的每個索引呼叫 `f`，最多使用 `threads` 條執行緒。
/// 索引以 AtomicUsize 動態分配，因此大小差異很大的工作也能自動平衡。
/// `threads == 0` 視為自動；`threads > n` 會被縮到 n。
/// 第一個錯誤會被記錄，之後不再啟動新的索引，但已在執行中的索引會跑完。
pub fn for_each<F>(n: usize, threads: usize, f: F) -> Result<()>
where
    F: Fn(usize) -> Result<()> + Sync,
{
    // 為什麼委派：無狀態版只是「狀態為空元組」的特例，共用同一套排程邏輯才不會分叉。
    for_each_with(n, threads, || (), |_, i| f(i))
}

/// 同上，但每條執行緒先用 `make()` 建立自己的狀態（例如可重用的緩衝區）。
pub fn for_each_with<T, M, F>(n: usize, threads: usize, make: M, f: F) -> Result<()>
where
    M: Fn() -> T + Sync,
    F: Fn(&mut T, usize) -> Result<()> + Sync,
{
    if n == 0 {
        return Ok(());
    }
    // 為什麼先解析再 clamp：threads == 0 是「自動」的語意，與「只用一條」不同，
    // 必須先換成真實核心數；之後縮到 [1, n] 保證至少跑一條、不開多餘執行緒。
    let hint = if threads == 0 {
        crate::sysio::available_parallelism()
    } else {
        threads
    };
    let num = hint.clamp(1, n.max(1));

    let next = AtomicUsize::new(0);
    // 為什麼是 Mutex<Option<Error>>：只保留第一個錯誤；scope 結束後主執行緒取出。
    // Error 本身是 Send（只含 String 與 io::Error），跨執行緒傳遞安全。
    let first_err: std::sync::Mutex<Option<crate::error::Error>> =
        std::sync::Mutex::new(None);

    // 為什麼用 scope 而不用 Arc + 'static：scope 允許工作執行緒直接借用呼叫端堆疊上的
    // next / first_err / make / f，不需要引用計數，也不會把 'static 約束傳染給呼叫端。
    std::thread::scope(|s| {
        for _ in 0..num {
            s.spawn(|| {
                let mut state = make();
                loop {
                    // 已有錯誤就不再領新索引；已領走的索引由持有它的執行緒跑完。
                    {
                        let guard = first_err
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if guard.is_some() {
                            break;
                        }
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= n {
                        break;
                    }
                    if let Err(e) = f(&mut state, idx) {
                        let mut guard = first_err
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        // 只保留第一個錯誤，後到的丟棄（呼叫端只關心「有錯」與第一個原因）。
                        if guard.is_none() {
                            *guard = Some(e);
                        }
                        break;
                    }
                }
            });
        }
    });

    let mut guard = first_err
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(e) = guard.take() {
        Err(e)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn empty_range_ok_without_running() {
        let calls = AtomicUsize::new(0);
        let r = for_each(0, 4, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(r.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // for_each_with 在 n == 0 時不建立任何執行緒狀態。
        let makes = AtomicUsize::new(0);
        let r = for_each_with(
            0,
            4,
            || {
                makes.fetch_add(1, Ordering::SeqCst);
            },
            |_: &mut (), _| Ok(()),
        );
        assert!(r.is_ok());
        assert_eq!(makes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn single_index_runs_once() {
        let hits = AtomicUsize::new(0);
        let r = for_each(1, 4, |i| {
            assert_eq!(i, 0);
            hits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(r.is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn threads_clamped_and_all_indices_covered() {
        // threads >> n：執行緒數會被縮到 n，每個索引恰好跑一次。
        let n = 3usize;
        let seen = std::sync::Mutex::new(vec![0usize; n]);
        let r = for_each(n, 64, |i| {
            let mut g = lock(&seen);
            if let Some(slot) = g.get_mut(i) {
                *slot += 1;
            }
            Ok(())
        });
        assert!(r.is_ok());
        let g = lock(&seen);
        assert_eq!(g.len(), n);
        for c in g.iter() {
            assert_eq!(*c, 1);
        }
    }

    #[test]
    fn threads_zero_means_auto() {
        let n = 32usize;
        let total = AtomicUsize::new(0);
        let r = for_each(n, 0, |_| {
            total.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(r.is_ok());
        assert_eq!(total.load(Ordering::SeqCst), n);
    }

    #[test]
    fn first_error_stops_new_indices_single_thread() {
        // 單執行緒時是確定性的：0,1,2 成功，3 失敗後不再啟動 4..n。
        let n = 10usize;
        let started = std::sync::Mutex::new(Vec::new());
        let r = for_each_with(
            n,
            1,
            Vec::<usize>::new,
            |_buf: &mut Vec<usize>, i| {
                lock(&started).push(i);
                if i == 3 {
                    return Err(crate::error::Error::BadFormat("boom".to_string()));
                }
                Ok(())
            },
        );
        let err = r.unwrap_err();
        assert_eq!(err.kind(), "bad-format");
        let g = lock(&started);
        assert_eq!(*g, vec![0, 1, 2, 3]);
    }

    #[test]
    fn error_propagates_with_multiple_threads() {
        // 多執行緒時只斷言整體回傳該錯誤（其他執行緒可能已領走索引並跑完）。
        let r = for_each(16, 4, |i| {
            if i == 7 {
                return Err(crate::error::Error::BadFormat("seven".to_string()));
            }
            Ok(())
        });
        let err = r.unwrap_err();
        assert_eq!(err.kind(), "bad-format");
    }

    #[test]
    fn each_thread_gets_independent_state() {
        // 每條執行緒呼叫一次 make；f 記錄 (state_id, index)，
        // 事後驗證每個索引恰好一次，且同一 state_id 的使用不互相干擾。
        let n = 40usize;
        let threads = 4usize;
        let make_calls = AtomicUsize::new(0);
        let next_id = AtomicUsize::new(0);
        let log = std::sync::Mutex::new(Vec::new());
        let r = for_each_with(
            n,
            threads,
            || {
                make_calls.fetch_add(1, Ordering::SeqCst);
                // 獨立的可變狀態：每個執行緒一份，內含該執行緒的唯一 id 與已處理數。
                (next_id.fetch_add(1, Ordering::SeqCst), 0usize)
            },
            |st: &mut (usize, usize), i| {
                st.1 += 1;
                lock(&log).push((st.0, i));
                Ok(())
            },
        );
        assert!(r.is_ok());
        // 每條 spawn 的執行緒恰好建立一份狀態。
        assert_eq!(make_calls.load(Ordering::SeqCst), threads);

        let g = lock(&log);
        assert_eq!(g.len(), n);
        // 每個索引恰好出現一次。
        let mut seen = vec![false; n];
        for (_, i) in g.iter() {
            assert!(!seen[*i], "index {i} ran twice");
            seen[*i] = true;
        }
        assert!(seen.iter().all(|b| *b));
        // state_id 只來自 make 配發的範圍，證明沒有執行緒共用或憑空捏造狀態。
        let made = make_calls.load(Ordering::SeqCst);
        for (id, _) in g.iter() {
            assert!(*id < made);
        }
    }
}
