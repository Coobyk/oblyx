//! Bounds how much RAM oblyx uses while converting.
//!
//! The budget (default 8 GB, override with `OBLYX_MEM_GB`) limits how many
//! files convert at once, and a new file only starts while the process's
//! resident memory is comfortably below the budget.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

const DEFAULT_BUDGET_GB: u64 = 8;
/// Rough worst-case resident memory of one conversion job (largest
/// inflated PDF stream + document + render buffers).
const PER_JOB_RESERVE: u64 = 2 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct MemBudget {
    bytes: u64,
}

impl MemBudget {
    /// Budget in bytes: `$OBLYX_MEM_GB` (whole GB, minimum 1), default 8.
    pub fn from_env() -> Self {
        let gb = std::env::var("OBLYX_MEM_GB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&g| g >= 1)
            .unwrap_or(DEFAULT_BUDGET_GB);
        Self {
            bytes: gb.saturating_mul(1024 * 1024 * 1024),
        }
    }

    pub fn bytes(self) -> u64 {
        self.bytes
    }

    /// How many files may convert at once (one slot ≈ 2.5 GB of budget).
    pub fn max_jobs(self, cpus: usize) -> usize {
        let by_mem = (self.bytes / PER_JOB_RESERVE).max(1) as usize;
        by_mem.min(cpus.max(1))
    }

    /// A new job may start only while RSS is below this watermark.
    fn watermark(self) -> u64 {
        self.bytes
            .saturating_sub(PER_JOB_RESERVE)
            .max(self.bytes / 2)
    }

    /// Resident set size of this process in bytes (0 if unknown).
    pub fn current_rss() -> u64 {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                return kb * 1024;
            }
        }
        0
    }
}

/// Caps concurrent conversion jobs and keeps total RSS under a [`MemBudget`].
pub struct JobLimiter {
    free: Mutex<usize>,
    cv: Condvar,
    budget: MemBudget,
}

impl JobLimiter {
    pub fn new(budget: MemBudget, cpus: usize) -> Self {
        Self {
            free: Mutex::new(budget.max_jobs(cpus)),
            cv: Condvar::new(),
            budget,
        }
    }

    /// Take a slot (waiting for one to free up), then wait until RSS is below
    /// the watermark, then run `f`.
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        let mut free = self.lock();
        while *free == 0 {
            free = self.cv.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        drop(free);

        while MemBudget::current_rss() > self.budget.watermark() {
            std::thread::sleep(Duration::from_millis(100));
        }

        let _release = SlotRelease(self);
        f()
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.free.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Returns a slot to the limiter even if the job panics.
struct SlotRelease<'a>(&'a JobLimiter);

impl Drop for SlotRelease<'_> {
    fn drop(&mut self) {
        let mut free = self.0.lock();
        *free += 1;
        drop(free);
        self.0.cv.notify_one();
    }
}
