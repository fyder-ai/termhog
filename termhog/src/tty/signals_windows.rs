//! Console control events on Windows, the nearest thing to signals.
//!
//! While a recording runs, Ctrl+C and Ctrl+Break mustn't end this process
//! before the child. A child sharing our console (its output through pipes)
//! gets the event from the console itself, as it would natively. A child on
//! a pseudoconsole gets Ctrl+C as the byte `0x03`, which its pseudoconsole
//! turns into the event, and raw mode keeps the key from reaching us at all.
//!
//! Ctrl+Break reaches us as the event even in raw mode, and has no byte of
//! its own. So it's forwarded to the pseudoconsole as the key press itself,
//! in win32-input-mode: an escape sequence ConPTY reads as a full key event,
//! raising Ctrl+Break for the child (see [`forward_breaks_to`]).
//!
//! Closing the console window, logoff and shutdown keep their default
//! action: the system ends every process on the console regardless.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once, PoisonError};

use windows_sys::Win32::Foundation::{BOOL, FALSE, TRUE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

/// How many recordings are running.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Keeps Ctrl+C from ending this process while held.
pub struct Guard(());

/// Start absorbing Ctrl+C and Ctrl+Break, until the guard drops.
pub fn watch() -> Guard {
    static INSTALL: Once = Once::new();
    // SAFETY: the handler is a plain function, which the system calls on a
    // thread of its own, and it only touches thread-safe statics.
    INSTALL.call_once(|| unsafe {
        SetConsoleCtrlHandler(Some(handler), TRUE);
    });
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    Guard(())
}

/// The pseudoconsole input Ctrl+Break is forwarded to, while a child runs on
/// one.
static BREAK_TARGET: Mutex<Option<File>> = Mutex::new(None);

/// Ctrl+Break pressed and released with the left Ctrl key held, as
/// win32-input-mode encodes key events: `ESC [`, then the virtual key, scan
/// code, character, down or up, modifier state and repeat count, then `_`.
/// Here that's VK_CANCEL (3), scan code 0x46, no character, down then up,
/// LEFT_CTRL_PRESSED (8), once. The Ctrl key itself goes around it.
const CTRL_BREAK_KEYS: &[u8] =
    b"\x1b[17;29;0;1;8;1_\x1b[3;70;0;1;8;1_\x1b[3;70;0;0;8;1_\x1b[17;29;0;0;0;1_";

/// Forward Ctrl+Break to `input`, a pseudoconsole's, until called with
/// `None`.
pub fn forward_breaks_to(input: Option<File>) {
    *BREAK_TARGET.lock().unwrap_or_else(PoisonError::into_inner) = input;
}

unsafe extern "system" fn handler(event: u32) -> BOOL {
    let absorbed = matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT);
    if !absorbed || ACTIVE.load(Ordering::SeqCst) == 0 {
        return FALSE;
    }
    if event == CTRL_BREAK_EVENT {
        let mut target = BREAK_TARGET.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(input) = target.as_mut() {
            let _ = input.write_all(CTRL_BREAK_KEYS);
        }
    }
    TRUE
}

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}
