//! The Windows host console: raw mode and its restoration.
//!
//! Raw mode here means console modes, not termios: input without line
//! editing, echo or Ctrl+C processing, delivered as VT sequences, and output
//! that interprets VT sequences as is. Both code pages are set to UTF-8,
//! since ConPTY's output is UTF-8.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};

use windows_sys::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleCP,
    GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, SetConsoleCP, SetConsoleMode,
    SetConsoleOutputCP,
};

const UTF8: u32 = 65001;

/// The console's input and output, opened by name so they're the console
/// itself even when stdin is redirected.
pub struct Console {
    pub input: File,
    pub output: File,
}

impl Console {
    pub fn open() -> io::Result<Console> {
        let open = |name| File::options().read(true).write(true).open(name);
        Ok(Console {
            input: open("CONIN$")?,
            output: open("CONOUT$")?,
        })
    }
}

/// Whether `handle` is a console (input or output).
pub fn is_console(handle: impl AsHandle) -> bool {
    mode(&handle).is_some()
}

fn mode(handle: &impl AsHandle) -> Option<CONSOLE_MODE> {
    let mut mode = 0;
    // SAFETY: `mode` is a valid out-pointer.
    let ok = unsafe { GetConsoleMode(handle.as_handle().as_raw_handle(), &mut mode) };
    (ok != 0).then_some(mode)
}

fn set_mode(handle: &impl AsHandle, mode: CONSOLE_MODE) -> io::Result<()> {
    // SAFETY: plain Win32 call on a borrowed handle.
    if unsafe { SetConsoleMode(handle.as_handle().as_raw_handle(), mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The visible size of a console output, as `(cols, rows)`. It's the window,
/// not the scrollback buffer behind it.
pub fn winsize(output: impl AsHandle) -> Option<(u16, u16)> {
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out-pointer.
    let ok = unsafe { GetConsoleScreenBufferInfo(output.as_handle().as_raw_handle(), &mut info) };
    if ok == 0 {
        return None;
    }
    let window = info.srWindow;
    let cols = (window.Right - window.Left + 1).max(0) as u16;
    let rows = (window.Bottom - window.Top + 1).max(0) as u16;
    (cols > 0 && rows > 0).then_some((cols, rows))
}

/// The console's settings from before raw mode, saved so the panic hook can
/// restore them without the guards. Input and output are restored apart:
/// input when the child is detached, output once nothing is left to write
/// to it (see [`OutputModeGuard`]).
static ORIGINAL: Mutex<Saved> = Mutex::new(Saved {
    input: None,
    output: None,
    owner: None,
});

struct Saved {
    input: Option<Before>,
    output: Option<Before>,
    /// The thread that entered raw mode, which owns the recording.
    owner: Option<ThreadId>,
}

/// One side of the console, with its mode and code page from before.
struct Before {
    console: File,
    mode: CONSOLE_MODE,
    code_page: u32,
}

/// Raw mode's input side: restores the console's input on drop.
pub struct RawModeGuard(());

/// Raw mode's output side: restores the console's output on drop. Whatever
/// writes the child's VT output to the console holds it, so the console
/// interprets that output until the last of it is written.
pub struct OutputModeGuard(());

impl RawModeGuard {
    pub fn enter(console: &Console) -> io::Result<(RawModeGuard, OutputModeGuard)> {
        let input_mode = mode(&console.input).ok_or_else(io::Error::last_os_error)?;
        let output_mode = mode(&console.output).ok_or_else(io::Error::last_os_error)?;
        // Cloned first, so nothing can fail once the console is raw.
        let saved = Saved {
            input: Some(Before {
                console: console.input.try_clone()?,
                mode: input_mode,
                // SAFETY: plain Win32 call.
                code_page: unsafe { GetConsoleCP() },
            }),
            output: Some(Before {
                console: console.output.try_clone()?,
                mode: output_mode,
                // SAFETY: plain Win32 call.
                code_page: unsafe { GetConsoleOutputCP() },
            }),
            owner: Some(thread::current().id()),
        };
        let raw_input = (input_mode
            & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        // ConPTY already sends `\r\n`, so the console mustn't add its own `\r`.
        let raw_output = output_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;
        *original() = saved;
        let entered = set_mode(&console.output, raw_output)
            .and_then(|()| set_mode(&console.input, raw_input));
        if let Err(e) = entered {
            restore_saved();
            return Err(e);
        }
        // SAFETY: plain Win32 calls.
        unsafe {
            SetConsoleCP(UTF8);
            SetConsoleOutputCP(UTF8);
        }
        Ok((RawModeGuard(()), OutputModeGuard(())))
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        restore_input(&mut original());
    }
}

impl Drop for OutputModeGuard {
    fn drop(&mut self) {
        restore_output(&mut original());
    }
}

/// The console's output code page set to UTF-8 while the child's output is
/// teed to it through a pipe (the bytes are written as they are), and put
/// back on drop.
pub struct Utf8Output(u32);

impl Utf8Output {
    pub fn set() -> Utf8Output {
        // SAFETY: plain Win32 calls.
        let previous = unsafe { GetConsoleOutputCP() };
        unsafe { SetConsoleOutputCP(UTF8) };
        Utf8Output(previous)
    }
}

impl Drop for Utf8Output {
    fn drop(&mut self) {
        // SAFETY: plain Win32 call.
        unsafe { SetConsoleOutputCP(self.0) };
    }
}

/// Run `f` with `\n` starting a new line again while raw mode is active, so
/// what it prints looks as it would natively. The saved state stays locked
/// meanwhile, so raw mode can't end halfway through and then be set again.
pub(super) fn with_output_processing(f: impl FnOnce()) {
    let saved = original();
    let raw = saved.output.as_ref().and_then(|output| {
        let raw = mode(&output.console)?;
        set_mode(&output.console, raw & !DISABLE_NEWLINE_AUTO_RETURN).ok()?;
        Some((&output.console, raw))
    });
    f();
    if let Some((console, raw)) = raw {
        let _ = set_mode(console, raw);
    }
}

/// Whether this thread entered the raw mode that's active.
pub(super) fn owns_terminal() -> bool {
    let saved = original();
    let active = saved.input.is_some() || saved.output.is_some();
    active && saved.owner == Some(thread::current().id())
}

/// Restore what's left of the saved console settings, if raw mode is
/// active, and hand back the console output. Safe to call from any thread,
/// and idempotent.
pub(super) fn restore_saved() -> Option<File> {
    let mut saved = original();
    restore_input(&mut saved);
    restore_output(&mut saved)
}

fn restore_input(saved: &mut Saved) {
    if let Some(input) = saved.input.take() {
        let _ = set_mode(&input.console, input.mode);
        // SAFETY: plain Win32 call.
        unsafe { SetConsoleCP(input.code_page) };
    }
}

/// Restore the output side, handing back the console output if it was set.
fn restore_output(saved: &mut Saved) -> Option<File> {
    let output = saved.output.take()?;
    let _ = set_mode(&output.console, output.mode);
    // SAFETY: plain Win32 call.
    unsafe { SetConsoleOutputCP(output.code_page) };
    Some(output.console)
}

fn original() -> MutexGuard<'static, Saved> {
    ORIGINAL.lock().unwrap_or_else(PoisonError::into_inner)
}
