//! Host terminal control: raw mode, its restoration, and the panic hook.
//!
//! The child's pty slave owns line discipline and signal generation. The host
//! terminal is put into raw mode so keystrokes (including `^C` = 0x03) flow
//! through untouched to the child. Restoration must survive normal return, `?`
//! propagation, a fatal panic, and signals, so a `Drop` guard, the panic hook
//! and signal handling all restore from the same saved termios.
//!
//! On Windows the pty is a ConPTY pseudoconsole and the host is the console
//! this process is attached to (see `windows.rs` and `conpty.rs`).

pub mod osc;
pub mod probe;
#[cfg_attr(windows, path = "conpty.rs")]
pub mod pty;
#[cfg_attr(windows, path = "signals_windows.rs")]
pub mod signals;
#[cfg(windows)]
pub mod spawner;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::io::Write;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// Escapes that undo screen state a full-screen child may have left on: show
/// the cursor, leave the alt screen, stop mouse reporting. Sent only when our
/// own panic cuts the normal teardown short. On a clean exit the child's own
/// cleanup has already reached the terminal, and a transparent wrapper leaves
/// exactly what running the child directly would.
const RECOVERY: &[u8] = b"\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// Set while a recording is attached to the terminal.
static TERMINAL_IN_USE: AtomicBool = AtomicBool::new(false);

/// Exclusive use of the controlling terminal. Two recordings can't both put it
/// in raw mode and read its input.
pub struct TerminalLease(());

impl TerminalLease {
    pub fn acquire() -> Option<TerminalLease> {
        TERMINAL_IN_USE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| TerminalLease(()))
    }
}

impl Drop for TerminalLease {
    fn drop(&mut self) {
        TERMINAL_IN_USE.store(false, Ordering::SeqCst);
    }
}

/// Install (once per process) a panic hook that runs before the previous
/// one. [`crate::init`] installs it first thing, so hooks installed later
/// (like posthog-rs's panic capture) run before it.
///
/// A panic that will end the process restores the terminal first, so the
/// panic message prints normally and the shell gets a sane terminal back.
/// That's a panic on the thread that owns the recording, or any panic when
/// panics abort. Other threads' panics may be caught, and the child is still
/// using the terminal, so they leave it raw, with output processing back on
/// just while their message prints. A recorder thread's panic only stops the
/// recording (see [`crate::util::spawn_recorder`]), so it's reported instead
/// of printed over the child's output.
pub fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let fatal = cfg!(panic = "abort");
            if crate::util::capture_recorder_panic(info) && !fatal {
                return;
            }
            if fatal || owns_terminal() {
                if let Some(mut tty) = restore_saved() {
                    let _ = tty.write_all(RECOVERY);
                    let _ = tty.flush();
                }
                previous(info);
            } else {
                with_output_processing(|| previous(info));
            }
        }));
    });
}
