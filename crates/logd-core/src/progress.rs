//! 后台任务的进度上报与取消信号。UI 线程只读，工作线程只写。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Progress {
    pub fn new(total: u64) -> Self {
        Self {
            done: AtomicU64::new(0),
            total: AtomicU64::new(total),
            cancel: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn set_total(&self, n: u64) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// 0.0..=1.0，total 为 0 时返回 1.0（视为已完成）。
    pub fn fraction(&self) -> f32 {
        let t = self.total();
        if t == 0 {
            return 1.0;
        }
        (self.done() as f64 / t as f64).clamp(0.0, 1.0) as f32
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}
