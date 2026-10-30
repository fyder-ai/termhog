//! Starting a child on a pseudoconsole through std.
//!
//! std can't attach a child to a pseudoconsole on stable Rust, and building
//! a Windows command line by hand is error-prone: a batch file's arguments
//! are parsed again by cmd.exe, where a mistake runs commands
//! (CVE-2024-24576). So this executable is started on the pseudoconsole
//! instead, as the spawner, and it starts the child with
//! `std::process::Command`. The child inherits the pseudoconsole, and std
//! does all quoting and program lookup.
//!
//! The command reaches the spawner as data over a pipe, never as a command
//! line. It replies with the child's process ID, and stays until we've
//! opened the child by that ID (so the ID can't be reused meanwhile), then
//! exits. From then on, the child is waited on and killed directly.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

use crate::HogCommand;

/// Set in the spawner's environment: the handle values of its request and
/// reply pipes, as `request,reply`. Not passed on to the child.
pub const VAR: &str = "TERMHOG_SPAWNER";

/// Bumped whenever the messages change, so a different build of this
/// executable (replaced while running) refuses rather than misreads them.
const VERSION: u32 = 1;

/// What the spawner is asked to start, as one line of JSON: the command,
/// minus its streams, and the handles it inherits for streams that aren't
/// on the pseudoconsole.
#[derive(Serialize, Deserialize)]
pub struct Request {
    version: u32,
    program: OsString,
    args: Vec<OsString>,
    env_clear: bool,
    envs: Vec<(OsString, Option<OsString>)>,
    current_dir: Option<OsString>,
    stdin: Option<usize>,
    stderr: Option<usize>,
}

impl Request {
    pub fn new(
        command: &HogCommand,
        stdin: Option<RawHandle>,
        stderr: Option<RawHandle>,
    ) -> Request {
        Request {
            version: VERSION,
            program: command.program.clone(),
            args: command.args.clone(),
            env_clear: command.env_clear,
            envs: command.envs.clone(),
            current_dir: command.current_dir.clone().map(PathBuf::into_os_string),
            stdin: stdin.map(|h| h as usize),
            stderr: stderr.map(|h| h as usize),
        }
    }

    /// The request as the spawner reads it: JSON, then a newline.
    pub fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).expect("a request always serializes");
        line.push(b'\n');
        line
    }
}

/// Why std couldn't start the child: its OS error code, or else its kind
/// and message.
#[derive(Serialize, Deserialize)]
struct Failure {
    os_error: Option<i32>,
    kind: Kind,
    message: String,
}

/// The kinds std gives the spawn errors it raises itself, without an OS
/// error code.
#[derive(Serialize, Deserialize)]
enum Kind {
    /// The program wasn't found.
    NotFound,
    /// An argument it can't pass on (like a NUL, or a batch file argument
    /// it can't escape).
    InvalidInput,
    Other,
}

impl From<io::ErrorKind> for Kind {
    fn from(kind: io::ErrorKind) -> Kind {
        match kind {
            io::ErrorKind::NotFound => Kind::NotFound,
            io::ErrorKind::InvalidInput => Kind::InvalidInput,
            _ => Kind::Other,
        }
    }
}

impl From<Kind> for io::ErrorKind {
    fn from(kind: Kind) -> io::ErrorKind {
        match kind {
            Kind::NotFound => io::ErrorKind::NotFound,
            Kind::InvalidInput => io::ErrorKind::InvalidInput,
            Kind::Other => io::ErrorKind::Other,
        }
    }
}

/// The child's process ID from the spawner's reply, or the error std
/// reported.
pub fn read_reply(reply: File) -> io::Result<u32> {
    let reply: Result<u32, Failure> = serde_json::from_reader(reply)
        .map_err(|e| io::Error::other(format!("the spawner didn't reply: {e}")))?;
    reply.map_err(|failure| match failure.os_error {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(failure.kind.into(), failure.message),
    })
}

/// Act as the spawner (see the module docs), then exit. `handles` is the
/// value of [`VAR`].
pub fn run(handles: &OsStr) -> ! {
    // Ctrl+C typed while we're still here is the child's alone, so it
    // mustn't end us before the handoff.
    let _absorb = super::signals::watch();
    std::process::exit(if serve(handles).is_ok() { 0 } else { 1 })
}

fn serve(handles: &OsStr) -> io::Result<()> {
    let invalid = || io::Error::other(format!("{VAR} is malformed"));
    let (request, reply) = handles
        .to_str()
        .and_then(|h| h.split_once(','))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .ok_or_else(invalid)?;
    let mut request = BufReader::new(File::from(take_handle(request)?));
    let mut reply = File::from(take_handle(reply)?);

    let mut line = String::new();
    request.read_line(&mut line)?;
    let outcome = serde_json::from_str::<Request>(&line)
        .map_err(io::Error::other)
        .and_then(spawn);
    let answer = outcome
        .as_ref()
        .map(|child| child.id())
        .map_err(|e| Failure {
            os_error: e.raw_os_error(),
            kind: e.kind().into(),
            message: e.to_string(),
        });
    serde_json::to_writer(&mut reply, &answer)?;
    drop(reply);
    // Stay until our parent has the child open. It closes the request pipe
    // then.
    let _ = request.read_to_end(&mut Vec::new());
    Ok(())
}

/// Start the requested command with std, its streams on the pseudoconsole
/// unless the request gave handles for them.
fn spawn(request: Request) -> io::Result<std::process::Child> {
    if request.version != VERSION {
        return Err(io::Error::other(
            "the spawner is a different build of this program",
        ));
    }
    let mut command = HogCommand::new(request.program);
    command.args = request.args;
    command.env_clear = request.env_clear;
    // Our own marker isn't the child's to see.
    if !request.env_clear {
        command.envs.push((VAR.into(), None));
    }
    command.envs.extend(request.envs);
    command.current_dir = request.current_dir.map(PathBuf::from);
    let mut std_cmd = command.to_std();
    if let Some(stdin) = request.stdin {
        std_cmd.stdin(take_handle(stdin)?);
    }
    if let Some(stderr) = request.stderr {
        std_cmd.stderr(take_handle(stderr)?);
    }
    std_cmd.spawn()
}

/// Own a handle we inherited, keeping it from being inherited further. std
/// passes the child its own copies of its streams.
fn take_handle(value: usize) -> io::Result<OwnedHandle> {
    let raw = value as RawHandle;
    // SAFETY: our parent put this handle in our handle table for us alone.
    if unsafe { SetHandleInformation(raw, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
