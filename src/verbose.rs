use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(0);

/// Set the global verbosity: 0 = off (default), 1 = `-v`, 2 = `-vv`.
pub fn set(level: u8) {
    LEVEL.store(level.min(2), Ordering::Relaxed);
}

/// Current verbosity level.
pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

/// Log `$msg` when the verbosity level is at least `$lvl`.
///
/// The level check is a single relaxed atomic load, so disabled logging costs
/// essentially nothing on hot paths.
#[macro_export]
macro_rules! vlog {
    ($lvl:expr, $($arg:tt)*) => {
        if $crate::verbose::level() >= $lvl {
            eprintln!("[oblyx] {}", format!($($arg)*));
        }
    };
}
