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

/// Console control events that ask for a clean exit. Ctrl+C is deliberately
/// not handled: in a raw console it arrives as a guest key (see
/// pane::key_bytes), and returning FALSE keeps the default Ctrl+C behaviour
/// for a non-raw console.
#[cfg(windows)]
unsafe extern "system" fn on_console_event(event: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match event {
        CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            // Same contract as the Unix handler: only set the flag and
            // return at once; the event loop does the cleanup. Never block
            // here: the OS kills a slow handler.
            REQUESTED.store(true, Ordering::SeqCst);
            1 // TRUE
        }
        _ => 0, // FALSE, including CTRL_C_EVENT.
    }
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
    #[cfg(windows)]
    {
        // SAFETY: `on_console_event` only stores to an atomic and returns
        // without blocking. A zero return means no handler is installed,
        // which is today's behaviour; keep running either way.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_event), 1);
        }
    }
}

pub(crate) fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    #[test]
    fn break_close_logoff_and_shutdown_ask_for_a_clean_exit_while_ctrl_c_is_left_alone() {
        for event in [
            CTRL_BREAK_EVENT,
            CTRL_CLOSE_EVENT,
            CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        ] {
            REQUESTED.store(false, Ordering::SeqCst);
            assert!(!requested());
            // SAFETY: direct call of the flag-setting handler.
            let handled = unsafe { on_console_event(event) };
            assert_eq!(handled, 1);
            assert!(requested(), "event {event} did not set the flag");
        }
        REQUESTED.store(false, Ordering::SeqCst);
        // SAFETY: direct call of the handler.
        let handled = unsafe { on_console_event(CTRL_C_EVENT) };
        assert_eq!(handled, 0);
        assert!(!requested());
        REQUESTED.store(false, Ordering::SeqCst);
    }
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
