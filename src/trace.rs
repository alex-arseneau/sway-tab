// Tiny replacement for tracing::trace! — a single global flag toggling
// stderr prints when `--trace` is passed on the command line.

use std::sync::atomic::AtomicBool;

pub static TRACE: AtomicBool = AtomicBool::new(false);

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}
