//! Ends Crowded the ordinary way when the terminal hangs up or it is told to
//! stop. A signal only sets a flag; the event loop sees it, leaves, and every
//! owned server is reaped by the same path as after Ctrl+Q.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Hang-up, interrupt and terminate: the same numbers on macOS and Linux.
#[cfg(unix)]
const SIGNALS: [i32; 3] = [1, 2, 15];

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

#[cfg(unix)]
extern "C" fn on_signal(_: i32) {
    // Storing to an atomic is all a signal handler may safely do.
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Routes the three signals to the flag. Without this the first of them ends the
/// process on the spot and leaves its OpenCode servers running. SIGKILL cannot
/// be caught and is not covered.
pub(crate) fn install() {
    #[cfg(unix)]
    for number in SIGNALS {
        // SAFETY: `on_signal` only stores to an atomic.
        unsafe {
            signal(number, on_signal as *const () as usize);
        }
    }
}

pub(crate) fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn raise(signum: i32) -> i32;
    }

    fn restore_defaults() {
        for number in SIGNALS {
            // SAFETY: restoring the default action (SIG_DFL is 0).
            unsafe {
                signal(number, 0);
            }
        }
    }

    #[test]
    fn hangup_interrupt_and_terminate_each_ask_for_a_clean_exit_instead_of_ending_the_process() {
        install();
        for (name, number) in [("hangup", 1), ("interrupt", 2), ("terminate", 15)] {
            REQUESTED.store(false, Ordering::SeqCst);
            assert!(!requested());
            // SAFETY: the handler installed above only sets a flag.
            unsafe {
                raise(number);
            }
            assert!(requested(), "{name} did not set the flag");
        }
        REQUESTED.store(false, Ordering::SeqCst);
        restore_defaults();
    }
}
