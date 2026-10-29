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
//! std can't attach a child to a pseudoconsole, so children are started here
//! with `CreateProcessW`, following std's rules for the command line,
//! environment and program lookup.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
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
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};

use super::Utf8Output;
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
/// Microsoft's license for the files, which goes wherever they do.
const CONPTY_LICENSE: &str = include_str!("../../ConPTY-LICENSE.txt");

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
        ("LICENSE.txt", CONPTY_LICENSE.as_bytes()),
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
    /// streams that aren't on it.
    pub fn spawn(&self, command: &HogCommand, stdio: &ChildStdio) -> io::Result<Child> {
        // Held until we return, so the pseudoconsole can't close meanwhile.
        let console = self.console.lock();
        let hpc = (*console).ok_or_else(|| io::Error::other("pseudoconsole closed"))?;
        let env = environment(command);
        let (program, mut cmdline) = program_and_command_line(command, env.as_deref())?;
        let program = wide(program.as_os_str());
        let block = env.map(|vars| env_block(&vars));
        let cwd = command.current_dir.as_ref().map(|d| wide(d.as_os_str()));

        // Inheritable copies of the child's own handles, closed once it has
        // them. The handle list keeps it from inheriting anything else.
        let input = stdio.input.as_ref().map(inheritable).transpose()?;
        let error = stdio.error.as_ref().map(inheritable).transpose()?;
        let handle_list: Vec<HANDLE> = [&input, &error]
            .into_iter()
            .flatten()
            .map(|h| h.as_raw_handle())
            .collect();
        let inherit = !handle_list.is_empty();
        let attributes = AttributeList::new(hpc, handle_list)?;
        // SAFETY: zeroed STARTUPINFOEXW is the documented starting point.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attributes.as_ptr();
        // Every stream is set: a null handle puts it on the pseudoconsole.
        // Left unset, a child of a process whose own stdio is redirected
        // would write to that redirect instead. INVALID_HANDLE_VALUE, which
        // works when all three are, left a console stream nowhere once
        // another stream had a real handle (Windows 11 26100).
        let raw = |h: &Option<OwnedHandle>| {
            h.as_ref()
                .map_or(std::ptr::null_mut(), |h| h.as_raw_handle())
        };
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = raw(&input);
        si.StartupInfo.hStdOutput = std::ptr::null_mut();
        si.StartupInfo.hStdError = raw(&error);

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call. Only the handles in
        // the list are inherited, and the pseudoconsole is an attribute.
        let ok = unsafe {
            CreateProcessW(
                program.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                inherit.into(),
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                block
                    .as_ref()
                    .map_or(std::ptr::null(), |b| b.as_ptr() as *const c_void),
                cwd.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles are ours now.
        unsafe { CloseHandle(pi.hThread) };
        Ok(Child::Console {
            process: unsafe { OwnedHandle::from_raw_handle(pi.hProcess) },
            pid: pi.dwProcessId,
            console: Arc::clone(&self.console),
        })
    }
}

/// The program `CreateProcessW` runs for `command`, found the way std finds
/// it in the child's PATH, and its command line. A batch file runs under
/// cmd.exe, which parses its command line again, so it gets cmd's escaping
/// too (see `batch_command_line`).
fn program_and_command_line(
    command: &HogCommand,
    env: Option<&[(OsString, OsString)]>,
) -> io::Result<(PathBuf, Vec<u16>)> {
    let path = env
        .and_then(|vars| vars.iter().find(|(k, _)| k.eq_ignore_ascii_case("PATH")))
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var_os("PATH"));
    let program = resolve(&command.program, path.as_deref())?;
    if is_batch(&program) {
        // cmd.exe would look for a relative script in the child's folder, so
        // it's made absolute against ours, as std does.
        let script = std::path::absolute(&program)?;
        return Ok((system_cmd()?, batch_command_line(&script, &command.args)?));
    }
    let cmdline = command_line(&program, &command.args)?;
    Ok((program, cmdline))
}

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

/// The child's environment, when the command changes it. `None` inherits
/// ours unchanged. Names match case-insensitively, as on Windows.
fn environment(command: &HogCommand) -> Option<Vec<(OsString, OsString)>> {
    if !command.env_clear && command.envs.is_empty() {
        return None;
    }
    let mut vars: Vec<(OsString, OsString)> = if command.env_clear {
        Vec::new()
    } else {
        std::env::vars_os().collect()
    };
    for (key, val) in &command.envs {
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
        if let Some(val) = val {
            vars.push((key.clone(), val.clone()));
        }
    }
    Some(vars)
}

/// Find `program` the way std does: a path is used as given, a bare name is
/// searched for in `path` (the child's PATH). Either gets `.exe` added when
/// it has no extension.
fn resolve(program: &OsStr, path: Option<&OsStr>) -> io::Result<PathBuf> {
    let with_exe = |p: PathBuf| -> PathBuf {
        if p.extension().is_some() {
            p
        } else {
            p.with_extension("exe")
        }
    };
    let candidate = Path::new(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return Ok(with_exe(candidate.to_path_buf()));
    }
    for dir in path.into_iter().flat_map(std::env::split_paths) {
        // An empty or relative entry (a trailing separator makes one) would
        // search the current folder, which std never does.
        if !dir.is_absolute() {
            continue;
        }
        let full = with_exe(dir.join(program));
        if full.is_file() {
            return Ok(full);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "program not found"))
}

/// The command line `CreateProcessW` takes, quoted so the child's C runtime
/// splits it back into exactly these arguments.
fn command_line(program: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    let program = program.as_os_str();
    if program.encode_wide().any(|c| c == u16::from(b'"')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "program path contains a quote",
        ));
    }
    line.push(u16::from(b'"'));
    line.extend(program.encode_wide());
    line.push(u16::from(b'"'));
    for arg in args {
        line.push(u16::from(b' '));
        append_quoted(&mut line, arg);
    }
    line.push(0);
    Ok(line)
}

/// Append `arg`, quoted if needed: backslashes are literal except before a
/// quote, where they and the quote are escaped.
fn append_quoted(line: &mut Vec<u16>, arg: &OsStr) {
    let arg: Vec<u16> = arg.encode_wide().collect();
    let special = |c: &u16| [b' ', b'\t', b'\n', 0x0b, b'"'].contains(&(*c as u8)) && *c < 0x80;
    if !arg.is_empty() && !arg.iter().any(special) {
        line.extend(&arg);
        return;
    }
    line.push(u16::from(b'"'));
    let mut backslashes = 0;
    for &c in &arg {
        if c == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if c == u16::from(b'"') {
                // Escape the backslashes before it, then the quote itself.
                line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
            }
            backslashes = 0;
        }
        line.push(c);
    }
    // Backslashes before the closing quote are doubled.
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
    line.push(u16::from(b'"'));
}

/// Whether `program` is a batch file, which Windows runs with cmd.exe.
fn is_batch(program: &Path) -> bool {
    program
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
}

/// The system's cmd.exe, by full path, so no other folder can supply one.
fn system_cmd() -> io::Result<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: `buf` holds `len` units.
    let len = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if len == 0 || len >= buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..len])).join("cmd.exe"))
}

/// The command line that runs `script` with `args` under cmd.exe, escaped
/// the way std does it (since Rust 1.77.2, for CVE-2024-24576), so no
/// argument can end the command and start another.
///
/// cmd.exe parses its command line itself, ignoring the C runtime's quoting
/// rules, and expands `%VAR%` even inside quotes. So, as std does: run with
/// command extensions on (`/e:ON`), delayed `!VAR!` expansion off
/// (`/v:OFF`), and no AutoRun commands (`/d`). Quote any argument that
/// isn't plainly safe, escape `"` as `""` so quotes stay balanced, and
/// escape `%` as `%%cd:~,%`, where `%cd:~,%` expands to nothing and leaves
/// the `%`. Line breaks would end the command, so they're refused.
fn batch_command_line(script: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    let invalid = |msg| io::Error::new(io::ErrorKind::InvalidInput, msg);
    let script: Vec<u16> = script.as_os_str().encode_wide().collect();
    if script.contains(&u16::from(b'"')) || script.last() == Some(&u16::from(b'\\')) {
        return Err(invalid(
            "batch file path contains a quote or ends with a backslash",
        ));
    }
    let mut line: Vec<u16> = "cmd.exe /e:ON /v:OFF /d /c \"".encode_utf16().collect();
    line.push(u16::from(b'"'));
    line.extend(&script);
    line.push(u16::from(b'"'));
    for arg in args {
        let arg: Vec<u16> = arg.encode_wide().collect();
        if arg
            .iter()
            .any(|&c| c == 0 || c == u16::from(b'\r') || c == u16::from(b'\n'))
        {
            return Err(invalid("batch file arguments are invalid"));
        }
        line.push(u16::from(b' '));
        // As std does: quote unless every ASCII character is known to be
        // safe, rather than trying to list every unsafe one. Also quote an
        // empty argument, or it vanishes, and one ending in `\`, or a script
        // using `"%~1"` gets its closing quote escaped.
        const UNQUOTED: &[u8] = b"#$*+-./:?@\\_";
        let needs_quotes = |c: char| {
            (c.is_ascii() && !(c.is_ascii_alphanumeric() || UNQUOTED.contains(&(c as u8))))
                || c.is_control()
        };
        let quote = arg.is_empty()
            || arg.last() == Some(&u16::from(b'\\'))
            || char::decode_utf16(arg.iter().copied()).any(|c| c.is_ok_and(needs_quotes));
        if quote {
            line.push(u16::from(b'"'));
        }
        let mut backslashes = 0;
        for &c in &arg {
            if c == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if c == u16::from(b'"') {
                    // Escape the backslashes before it for the script's own
                    // argument parsing, then double the quote for cmd.
                    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
                    line.push(u16::from(b'"'));
                }
                backslashes = 0;
            }
            if c == u16::from(b'%') {
                line.extend("%%cd:~,".encode_utf16());
            }
            line.push(c);
        }
        if quote {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            line.push(u16::from(b'"'));
        }
    }
    // Close the quote that wraps the whole command.
    line.push(u16::from(b'"'));
    line.push(0);
    Ok(line)
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

    fn quoted(arg: &str) -> String {
        let mut line = Vec::new();
        append_quoted(&mut line, OsStr::new(arg));
        String::from_utf16(&line).unwrap()
    }

    #[test]
    fn quoting_follows_the_c_runtime() {
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("two words"), "\"two words\"");
        assert_eq!(quoted("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quoted("C:\\dir\\"), "C:\\dir\\");
        assert_eq!(quoted("C:\\my dir\\"), "\"C:\\my dir\\\\\"");
        assert_eq!(quoted("a\\\\\"b"), "\"a\\\\\\\\\\\"b\"");
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
    fn env_changes_match_names_case_insensitively() {
        let mut command = HogCommand::new("x");
        command.env_clear().env("Path", "a").env("PATH", "b");
        let vars = environment(&command).unwrap();
        assert_eq!(vars, vec![(OsString::from("PATH"), OsString::from("b"))]);
    }

    #[test]
    fn missing_program_is_not_found() {
        let err = resolve(OsStr::new("no-such-program-xyz"), Some(OsStr::new("C:\\"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
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

    #[test]
    fn batch_arguments_are_escaped_for_cmd() {
        let line = batch_command_line(
            Path::new("C:\\t\\x.bat"),
            &[
                OsString::from("a b"),
                OsString::from("\"&calc"),
                OsString::from("5%"),
            ],
        )
        .unwrap();
        let line = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(
            line,
            "cmd.exe /e:ON /v:OFF /d /c \"\"C:\\t\\x.bat\" \"a b\" \"\"\"&calc\" \"5%%cd:~,%\"\""
        );
        for bad in ["a\nb", "a\rb", "a\0b"] {
            assert!(batch_command_line(Path::new("x.bat"), &[OsString::from(bad)]).is_err());
        }
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
