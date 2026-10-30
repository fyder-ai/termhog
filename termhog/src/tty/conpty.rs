//! Pseudoconsole plumbing on Windows: a ConPTY in place of a pty, and spawning
//! a child attached to it.
//!
//! Windows' own ConPTY re-renders the child's output, so what reaches the
//! terminal and the recording isn't the bytes the child wrote. The ConPTY
//! that ships with Windows Terminal (`conpty.dll` and `OpenConsole.exe`, from
//! Microsoft's `Microsoft.Windows.Console.ConPTY` package) passes VT output
//! through untouched. It's embedded in the library (see `build.rs`) and is
//! the only one used: Windows can only load and start it from files, so it's
//! written once into the user's cache folder.
//!
//! std can't attach a child to a pseudoconsole on stable Rust, so a copy of
//! this program is started on it, and starts the child with std (see
//! `spawner`).

use std::ffi::{OsString, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, S_OK,
};
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};

use super::Utf8Output;
use super::spawner::{self, Request};
use crate::HogCommand;
use crate::util::{self, env_block, wide};

/// Start the child at the terminal's cursor. Its value is the same in every
/// ConPTY version, unlike the higher flags.
const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;

type CreateFn = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizeFn = unsafe extern "system" fn(HPCON, COORD) -> i32;
type CloseFn = unsafe extern "system" fn(HPCON);

/// The embedded ConPTY's package version, and its files (see `build.rs`).
const CONPTY_VERSION: &str = env!("TERMHOG_CONPTY_VERSION");
const CONPTY_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/conpty.dll"));
const OPENCONSOLE_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/OpenConsole.exe"));

/// The ConPTY functions, from the embedded `conpty.dll`, and the folder it
/// was installed in.
struct Api {
    create: CreateFn,
    resize: ResizeFn,
    close: CloseFn,
    dir: PathBuf,
}

/// The embedded ConPTY, installed into `root` (the user's cache folder when
/// `None`) and loaded on first use. Once it's loaded, later calls get the
/// same one, whatever their `root`. A failure isn't kept, so a later call can
/// succeed. There's no fallback to Windows' own: an error fails the
/// recording.
fn api(root: Option<&Path>) -> io::Result<&'static Api> {
    static API: OnceLock<Api> = OnceLock::new();
    if let Some(api) = API.get() {
        return Ok(api);
    }
    let root = match root {
        Some(root) => root.to_path_buf(),
        None => dirs::cache_dir().ok_or_else(|| io::Error::other("no cache folder"))?,
    };
    let dir = root.join("termhog").join("conpty").join(CONPTY_VERSION);
    install(&dir)
        .map_err(|e| io::Error::other(format!("couldn't write Microsoft's ConPTY to disk: {e}")))?;
    let api = load(dir)?;
    // Two threads loading at once get the same library, so either is fine.
    Ok(API.get_or_init(|| api))
}

/// Write the embedded ConPTY into `dir`, unless it's already there
/// unchanged. `conpty.dll` starts the `OpenConsole.exe` beside it, so both go
/// there.
///
/// Files already present are compared byte for byte before they're used,
/// so a damaged or replaced copy is rewritten, never loaded. Another process
/// may be writing (or already running) the same files: each is written to a
/// temporary file and renamed into place, and a rename that fails because
/// the file is in use is fine if what's there is right.
fn install(dir: &Path) -> io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let files = [
        ("conpty.dll", CONPTY_DLL),
        ("OpenConsole.exe", OPENCONSOLE_EXE),
    ];
    for (name, bytes) in files {
        let path = dir.join(name);
        let intact = || {
            let same_size = std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64);
            same_size && std::fs::read(&path).is_ok_and(|on_disk| on_disk == bytes)
        };
        if intact() {
            continue;
        }
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        temp.write_all(bytes)?;
        if let Err(e) = temp.persist(&path) {
            if !intact() {
                return Err(e.error);
            }
        }
    }
    Ok(())
}

/// Load the `conpty.dll` installed in `dir`.
fn load(dir: PathBuf) -> io::Result<Api> {
    let dll = dir.join("conpty.dll");
    let failed = || {
        let e = io::Error::last_os_error();
        io::Error::other(format!(
            "couldn't load Microsoft's ConPTY ({}): {e}",
            dll.display()
        ))
    };
    let name = wide(dll.as_os_str());
    // SAFETY: loading a library by full path, just checked to be the
    // embedded one, and looking up functions whose signatures are fixed by
    // the ConPTY API.
    unsafe {
        let module = LoadLibraryW(name.as_ptr());
        if module.is_null() {
            return Err(failed());
        }
        let find = |name: &std::ffi::CStr| GetProcAddress(module, name.as_ptr().cast());
        let (Some(create), Some(resize), Some(close)) = (
            find(c"CreatePseudoConsole"),
            find(c"ResizePseudoConsole"),
            find(c"ClosePseudoConsole"),
        ) else {
            return Err(failed());
        };
        type Proc = unsafe extern "system" fn() -> isize;
        Ok(Api {
            create: std::mem::transmute::<Proc, CreateFn>(create),
            resize: std::mem::transmute::<Proc, ResizeFn>(resize),
            close: std::mem::transmute::<Proc, CloseFn>(close),
            dir,
        })
    }
}

/// A pseudoconsole, closed once. Its output pipe only ends once it's
/// closed, so the reader must be draining it by then, or closing blocks.
pub struct PseudoConsole {
    api: &'static Api,
    hpc: Mutex<Option<HPCON>>,
    /// Where the child's keyboard input is written. It stays open as long as
    /// the pseudoconsole, because ConPTY ends every process on it once its
    /// input closes.
    pub input: File,
}

// SAFETY: an HPCON is a handle usable from any thread, and access is locked.
unsafe impl Send for PseudoConsole {}
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    /// Resize to `(cols, rows)`. The child sees a window-size change.
    pub fn resize(&self, (cols, rows): (u16, u16)) -> io::Result<()> {
        let hpc = self.lock();
        let Some(open) = *hpc else {
            return Ok(());
        };
        // SAFETY: `open` stays open while the lock is held, until we return.
        let hr = unsafe { (self.api.resize)(open, coord((cols, rows))) };
        hresult(hr)
    }

    /// Close the pseudoconsole, ending its output once drained. Anything
    /// still attached to it is ended too.
    pub fn close(&self) {
        if let Some(hpc) = self.lock().take() {
            // SAFETY: taken out above, so it's closed exactly once.
            unsafe { (self.api.close)(hpc) };
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<HPCON>> {
        self.hpc.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        self.close();
    }
}

/// An open pseudoconsole with our ends of its pipes.
pub struct Pty {
    pub console: Arc<PseudoConsole>,
    /// Where the child's rendered output is read.
    pub output: File,
}

/// Open a pseudoconsole of the given size, on the embedded ConPTY, which is
/// written into `executable_dir` (see [`crate::TermHog::executable_dir`]) if
/// this is the first. Fails when it can't be installed or loaded.
///
/// With `inherit_cursor`, the child starts where the terminal's cursor is,
/// rather than at the top left, overwriting what's above. ConPTY then asks
/// the terminal where that is (`ESC [ 6 n` in its output) and waits for the
/// answer on its input, so the output must reach a terminal and the
/// terminal's input must be forwarded, as an attached recording does.
pub fn open(
    size: (u16, u16),
    inherit_cursor: bool,
    executable_dir: Option<&Path>,
) -> io::Result<Pty> {
    let api = api(executable_dir)?;
    // conpty.dll starts its OpenConsole.exe from disk for every
    // pseudoconsole, so the files are checked again (and rewritten if
    // they've changed) each time, not just when the DLL was first loaded.
    install(&api.dir)?;
    let (input_read, input) = util::pipe()?;
    let (output, output_write) = util::pipe()?;
    let flags = if inherit_cursor {
        PSEUDOCONSOLE_INHERIT_CURSOR
    } else {
        0
    };
    let mut hpc: HPCON = 0 as HPCON;
    // SAFETY: the pipe ends are valid, and ConPTY duplicates what it keeps,
    // so ours are closed when they drop below.
    let hr = unsafe {
        (api.create)(
            coord(size),
            input_read.as_raw_handle(),
            output_write.as_raw_handle(),
            flags,
            &mut hpc,
        )
    };
    hresult(hr)?;
    Ok(Pty {
        console: Arc::new(PseudoConsole {
            api,
            hpc: Mutex::new(Some(hpc)),
            input: File::from(input),
        }),
        output,
    })
}

/// A read from a pseudoconsole (or a pipe) whose writer has gone: it's been
/// closed, or the child exited. Treat it as EOF.
pub fn is_eof_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::BrokenPipe
}

fn coord((cols, rows): (u16, u16)) -> COORD {
    COORD {
        X: cols.min(i16::MAX as u16) as i16,
        Y: rows.min(i16::MAX as u16) as i16,
    }
}

fn hresult(hr: i32) -> io::Result<()> {
    if hr == S_OK {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(hr))
    }
}

/// What a child started on a pseudoconsole gets for stdin and stderr: a file
/// or pipe of its own, or `None` for the pseudoconsole. Its stdout is always
/// the pseudoconsole, which a child only gets for a stdout on the console.
#[derive(Default)]
pub struct ChildStdio {
    pub input: Option<OwnedHandle>,
    pub error: Option<OwnedHandle>,
}

impl Pty {
    /// Start `command` attached to the pseudoconsole, with `stdio` for any
    /// streams that aren't on it. std starts it, in a copy of this program
    /// run on the pseudoconsole as the spawner (see `spawner`).
    pub fn spawn(&self, command: &HogCommand, stdio: &ChildStdio) -> io::Result<Child> {
        let (request_end, request_write) = util::pipe()?;
        let mut request_write = File::from(request_write);
        let (reply_read, reply_end) = util::pipe()?;
        // Inheritable copies of what the spawner gets, closed once it has
        // them. The handle list keeps it from inheriting anything else. Our
        // originals of its pipe ends are closed now, or a pipe would never
        // end.
        let request_read = inheritable(&request_end.into())?;
        let reply_write = inheritable(&reply_end)?;
        drop(reply_end);
        let input = stdio.input.as_ref().map(inheritable).transpose()?;
        let error = stdio.error.as_ref().map(inheritable).transpose()?;
        let request = Request::new(
            command,
            input.as_ref().map(|h| h.as_raw_handle()),
            error.as_ref().map(|h| h.as_raw_handle()),
        );
        let handles = format!(
            "{},{}",
            request_read.as_raw_handle() as usize,
            reply_write.as_raw_handle() as usize
        );
        let handle_list = [
            Some(&request_read),
            Some(&reply_write),
            input.as_ref(),
            error.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|h| h.as_raw_handle())
        .collect();
        let spawner = self.start_spawner(&handles, handle_list)?;
        drop((request_read, reply_write, input, error));

        request_write.write_all(&request.to_line())?;
        let pid = spawner::read_reply(reply_read)?;
        // SAFETY: opening a process by ID. The spawner holds the child open
        // until we close the request pipe below, so the ID is still its.
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Lets the spawner exit.
        drop(request_write);
        drop(spawner);
        Ok(Child::Console {
            process: unsafe { OwnedHandle::from_raw_handle(process) },
            pid,
            console: Arc::clone(&self.console),
        })
    }

    /// Start this program on the pseudoconsole as the spawner, with
    /// `handles` (the request and reply pipes) for it to find and `inherit`
    /// for it to inherit. Its streams are all the pseudoconsole's.
    fn start_spawner(&self, handles: &str, inherit: Vec<HANDLE>) -> io::Result<OwnedHandle> {
        // Held until we return, so the pseudoconsole can't close meanwhile.
        let console = self.console.lock();
        let hpc = (*console).ok_or_else(|| io::Error::other("pseudoconsole closed"))?;
        let exe = std::env::current_exe()?;
        let program = wide(exe.as_os_str());
        // Its command line is just the program, quoted.
        let mut cmdline: Vec<u16> = std::iter::once(u16::from(b'"'))
            .chain(exe.as_os_str().encode_wide())
            .chain(std::iter::once(u16::from(b'"')))
            .chain(SPAWNER_ARGS.encode_utf16())
            .chain(std::iter::once(0))
            .collect();
        let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| !key.eq_ignore_ascii_case(spawner::VAR))
            .collect();
        vars.push((spawner::VAR.into(), handles.into()));
        let block = env_block(&vars);

        let attributes = AttributeList::new(hpc, inherit)?;
        // SAFETY: zeroed STARTUPINFOEXW is the documented starting point.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attributes.as_ptr();
        // Null handles put every stream on the pseudoconsole. Left unset, a
        // child of a process whose own stdio is redirected would write to
        // that redirect instead.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call. Only the handles in
        // the list are inherited, and the pseudoconsole is an attribute.
        let ok = unsafe {
            CreateProcessW(
                program.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr() as *const c_void,
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles are ours now.
        unsafe { CloseHandle(pi.hThread) };
        Ok(unsafe { OwnedHandle::from_raw_handle(pi.hProcess) })
    }
}

/// Arguments after the program on the spawner's command line. The test
/// harness runs only the test that acts as the spawner.
#[cfg(not(test))]
const SPAWNER_ARGS: &str = "";
#[cfg(test)]
const SPAWNER_ARGS: &str = " tty::pty::tests::spawner --exact --quiet";

/// A process thread attribute list holding a pseudoconsole, and the handles
/// the child may inherit when there are any. The list points into `handles`,
/// so they live together.
struct AttributeList {
    /// u64s keep the buffer aligned for the pointers inside.
    buffer: Vec<u64>,
    handles: Vec<HANDLE>,
}

impl AttributeList {
    fn new(hpc: HPCON, handles: Vec<HANDLE>) -> io::Result<AttributeList> {
        let count = if handles.is_empty() { 1 } else { 2 };
        let mut size = 0;
        // SAFETY: the first call only reports the size needed.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let ptr = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: the buffer is at least `size` bytes.
        if unsafe { InitializeProcThreadAttributeList(ptr, count, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Initialized, so dropping it deletes the list from here on.
        let list = AttributeList { buffer, handles };
        // The pseudoconsole attribute takes the HPCON value itself, not a
        // pointer to it.
        list.update(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc as *const c_void,
            std::mem::size_of::<HPCON>(),
        )?;
        if !list.handles.is_empty() {
            list.update(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                list.handles.as_ptr() as *const c_void,
                std::mem::size_of_val(list.handles.as_slice()),
            )?;
        }
        Ok(list)
    }

    fn update(&self, attribute: u32, value: *const c_void, size: usize) -> io::Result<()> {
        // SAFETY: the list is initialized, and `value` outlives it (it's
        // either a plain value or points into `self.handles`).
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// An inheritable copy of `handle`, for a child to be given.
fn inheritable(handle: &OwnedHandle) -> io::Result<OwnedHandle> {
    let mut copy = std::ptr::null_mut();
    // SAFETY: duplicating an open handle within this process.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.as_raw_handle(),
            GetCurrentProcess(),
            &mut copy,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the copy is ours.
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}

/// The recorded child: started by std (its output through pipes), or on a
/// pseudoconsole. Mirrors [`std::process::Child`]'s waiting and killing.
pub enum Child {
    Std {
        child: std::process::Child,
        /// The console's code page, UTF-8 while the child's output is teed
        /// to it through a pipe. Put back when this drops, as the recording
        /// ends.
        _utf8_console: Option<Utf8Output>,
    },
    Console {
        process: OwnedHandle,
        pid: u32,
        console: Arc<PseudoConsole>,
    },
}

impl AsHandle for Child {
    /// The process, which is signaled once it exits.
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Child::Std { child, .. } => child.as_handle(),
            Child::Console { process, .. } => process.as_handle(),
        }
    }
}

impl Child {
    pub fn id(&self) -> u32 {
        match self {
            Child::Std { child, .. } => child.id(),
            Child::Console { pid, .. } => *pid,
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        match self {
            Child::Std { child, .. } => child.kill(),
            Child::Console { process, .. } => {
                // SAFETY: the handle is open. Like std, an already exited
                // process isn't an error.
                if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0
                    && !util::wait_handle(&*process, 0)?
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }
    }

    /// Wait for the child to exit. A pseudoconsole's output never ends while
    /// it's open, even with the child gone, so it's closed then: what's left
    /// is flushed and the pipe ends. Anything the child left running on it
    /// ends too, as when a console window closes.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            Child::Std { child, .. } => child.wait(),
            Child::Console {
                process, console, ..
            } => {
                util::wait_handle(&*process, INFINITE)?;
                let mut code = 0;
                // SAFETY: the handle is open, and `code` is an out-pointer.
                if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                console.close();
                Ok(ExitStatus::from_raw(code))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// The spawner, when the harness is started as one (see `SPAWNER_ARGS`).
    /// Otherwise it does nothing.
    #[test]
    fn spawner() {
        if let Some(handles) = std::env::var_os(spawner::VAR) {
            spawner::run(&handles);
        }
    }

    #[test]
    fn env_block_is_sorted_and_terminated() {
        let vars = vec![
            (OsString::from("b"), OsString::from("2")),
            (OsString::from("A"), OsString::from("1")),
        ];
        let block = String::from_utf16(&env_block(&vars)).unwrap();
        assert_eq!(block, "A=1\0b=2\0\0");
        assert_eq!(env_block(&[]), vec![0, 0]);
    }

    #[test]
    fn spawn_errors_come_back_as_std_reported_them() {
        let pty = open((80, 24), false, None).unwrap();
        let kind = |command: &HogCommand| {
            let result = pty.spawn(command, &ChildStdio::default());
            result.err().expect("the spawn should fail").kind()
        };
        assert_eq!(
            kind(&HogCommand::new("no-such-program-xyz")),
            io::ErrorKind::NotFound
        );
        // One std refuses itself, without an OS error.
        let mut command = HogCommand::new("cmd");
        command.arg("a\0b");
        assert_eq!(kind(&command), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn child_runs_on_the_pseudoconsole() {
        // No terminal answers here, so the cursor can't be inherited.
        let pty = open((80, 24), false, None).unwrap();
        let mut command = HogCommand::new("cmd");
        command.args(["/c", "echo pseudoconsole-ok& exit 5"]);
        let mut child = pty.spawn(&command, &ChildStdio::default()).unwrap();
        let Pty {
            console, output, ..
        } = pty;
        // Drain while waiting, or closing could block.
        let reader = std::thread::spawn(move || {
            let mut output = output;
            let mut bytes = Vec::new();
            let _ = output.read_to_end(&mut bytes);
            bytes
        });
        assert_eq!(child.wait().unwrap().code(), Some(5));
        console.close();
        let text = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
        assert!(text.contains("pseudoconsole-ok"), "output: {text:?}");
    }

    /// Runs a batch file that records its arguments, through std (the
    /// reference) and through the pseudoconsole, with arguments that try to
    /// break out of cmd's quoting. Both must record the same thing, and
    /// nothing may run.
    #[test]
    fn batch_files_get_the_same_arguments_as_from_std() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("args.bat");
        std::fs::write(&script, "@echo off\r\n>>\"%~dp0out.txt\" echo(%*\r\n").unwrap();
        let args = [
            "plain",
            "two words",
            "",
            "\"&echo pwned>pwned.txt",
            "&echo pwned>pwned.txt",
            "\"&echo pwned>pwned.txt&\"",
            "%PATH%",
            "100%",
            "quote\"inside",
            "trailing\\",
            "^caret",
            "!bang!",
            "a|b<c>d",
        ];
        let out = dir.path().join("out.txt");
        let pwned = dir.path().join("pwned.txt");

        let status = std::process::Command::new(&script)
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let from_std = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();

        let pty = open((80, 24), false, None).unwrap();
        let mut command = HogCommand::new(&script);
        command.args(args).current_dir(dir.path());
        let mut child = pty.spawn(&command, &ChildStdio::default()).unwrap();
        let Pty {
            console, output, ..
        } = pty;
        let reader = std::thread::spawn(move || {
            let mut output = output;
            let _ = output.read_to_end(&mut Vec::new());
        });
        child.wait().unwrap();
        console.close();
        reader.join().unwrap();
        let from_pty = std::fs::read_to_string(&out).unwrap();

        assert_eq!(from_pty, from_std);
        assert!(!pwned.exists(), "an argument ran a command");
    }

    #[test]
    fn redirected_streams_bypass_the_pseudoconsole() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.txt");
        std::fs::write(&input, "from-file\r\n").unwrap();
        let (mut err_read, err_write) = util::pipe().unwrap();
        let stdio = ChildStdio {
            input: Some(File::open(&input).unwrap().into()),
            error: Some(err_write),
        };

        let pty = open((80, 24), false, None).unwrap();
        let mut command = HogCommand::new("cmd");
        // Delayed expansion, so `!X!` is read after `set /p` sets it.
        command.args([
            "/v:on",
            "/c",
            "set /p X=& echo got-!X!& echo to-stderr 1>&2",
        ]);
        let mut child = pty.spawn(&command, &stdio).unwrap();
        // Ours too, or the stderr pipe never ends.
        drop(stdio);
        let Pty {
            console, output, ..
        } = pty;
        let reader = std::thread::spawn(move || {
            let mut output = output;
            let mut bytes = Vec::new();
            let _ = output.read_to_end(&mut bytes);
            bytes
        });
        child.wait().unwrap();
        console.close();
        let screen = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
        let mut stderr = String::new();
        err_read.read_to_string(&mut stderr).unwrap();
        assert!(screen.contains("got-from-file"), "screen: {screen:?}");
        assert!(!screen.contains("to-stderr"), "screen: {screen:?}");
        assert!(stderr.contains("to-stderr"), "stderr: {stderr:?}");
    }
}
