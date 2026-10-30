//! Starting the child on Windows.
//!
//! When stdout is on the console, the child gets a pseudoconsole for it, and
//! for stdin and stderr when they're on the console too. Otherwise std starts
//! it, exactly as configured, and inherited output goes through pipes we tee.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::process::{ChildStderr, ChildStdin, Stdio};
use std::time::Instant;

use super::Started;
use super::pump::{Attachment, PtyOutput};
use crate::command::StdioKind;
use crate::tty;
use crate::tty::pty::{Child, ChildStdio};
use crate::util;
use crate::{Error, HogCommand, HogStdio, Result, TermHog};

/// Start `command`, on a pseudoconsole when stdout is on the console.
pub(super) fn start_child(settings: &TermHog, command: &HogCommand) -> Result<Started> {
    let out_console = command.stdout.is_inherit() && tty::is_console(io::stdout());
    let err_console = command.stderr.is_inherit() && tty::is_console(io::stderr());
    let in_console = command.stdin.is_inherit() && tty::is_console(io::stdin());
    let spawn_error = |source| Error::Spawn {
        program: command.program.clone(),
        source,
    };

    // Only take over the console when stdout is on it. When just stderr is
    // (e.g. `termhog -- make | more`), stderr is recorded through a pipe.
    let host = if out_console {
        Attachment::open(settings.fallback_size, settings.executable_dir.as_deref())?
    } else {
        None
    };
    if let Some((attachment, pty, output_mode)) = host {
        let (stdio, ends) = redirects(command, in_console, err_console)?;
        let child = match pty.spawn(command, &stdio) {
            Ok(child) => child,
            Err(source) => {
                attachment.abandon();
                return Err(spawn_error(source));
            }
        };
        let start = Instant::now();
        // Drop the child's ends we held, so readers see EOF.
        drop(stdio);
        let tees = Vec::from_iter(ends.stderr_tee.map(|pipe| (pipe, true)));
        return Ok(Started {
            child,
            start,
            stdin: ends.stdin,
            stdout: None,
            stderr: ends.stderr,
            size: attachment.size,
            attachment: Some(attachment),
            pty_output: Some(PtyOutput::new(pty.output, output_mode)),
            tees,
        });
    }

    let size = out_console
        .then(|| tty::winsize(io::stdout()))
        .flatten()
        .unwrap_or(settings.fallback_size);
    let (tee_stdout, tee_stderr) = (command.stdout.is_inherit(), command.stderr.is_inherit());
    // Both on the console, or the same file: one pipe, read in order, so
    // they interleave as they would there.
    let merged = tee_stdout
        && tee_stderr
        && ((out_console && err_console) || util::same_file(io::stdout(), io::stderr()));
    let mut std_cmd = command.to_std();
    std_cmd.stdin(command.stdin.to_std()?);
    let mut tees = Vec::new();
    if merged {
        let (read, write) = util::pipe()?;
        std_cmd.stdout(write.try_clone()?).stderr(write);
        tees.push((read, false));
    } else {
        let stdio = |tee: bool, spec: &HogStdio| -> io::Result<Stdio> {
            if tee {
                Ok(Stdio::piped())
            } else {
                spec.to_std()
            }
        };
        std_cmd.stdout(stdio(tee_stdout, &command.stdout)?);
        std_cmd.stderr(stdio(tee_stderr, &command.stderr)?);
    }
    // Output teed to the console is written to it as bytes, and the bytes
    // are UTF-8 (the recording reads them so too) for any program that
    // follows the console's code page, once that's UTF-8. Set before the
    // child starts, so it sees it.
    let utf8_console = (out_console || err_console).then(tty::Utf8Output::set);
    let mut child = std_cmd.spawn().map_err(spawn_error)?;
    let start = Instant::now();
    // Drop our copies of the pipe write ends, so readers see EOF.
    drop(std_cmd);
    if !merged && tee_stdout {
        let pipe = OwnedHandle::from(child.stdout.take().expect("piped stdout"));
        tees.push((File::from(pipe), false));
    }
    if !merged && tee_stderr {
        let pipe = OwnedHandle::from(child.stderr.take().expect("piped stderr"));
        tees.push((File::from(pipe), true));
    }
    Ok(Started {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child: Child::Std {
            child,
            _utf8_console: utf8_console,
        },
        start,
        attachment: None,
        pty_output: None,
        tees,
        size,
    })
}

/// Our ends of an attached child's stdin and stderr, when they aren't on the
/// console.
#[derive(Default)]
struct Ends {
    stdin: Option<ChildStdin>,
    stderr: Option<ChildStderr>,
    /// Inherited stderr that isn't on the console, to tee to ours.
    stderr_tee: Option<File>,
}

/// What a child on the pseudoconsole gets for stdin and stderr, as on Unix:
/// a stream inherited from the console goes on the pseudoconsole, a
/// redirected one goes to the child untouched, and inherited stderr off the
/// console is teed through a pipe so it's still recorded.
fn redirects(
    command: &HogCommand,
    in_console: bool,
    err_console: bool,
) -> io::Result<(ChildStdio, Ends)> {
    let mut stdio = ChildStdio::default();
    let mut ends = Ends::default();
    stdio.input = match &command.stdin.0 {
        StdioKind::Inherit if in_console => None,
        // No stdin at all (a detached process) leaves it on the pseudoconsole.
        StdioKind::Inherit => io::stdin().as_handle().try_clone_to_owned().ok(),
        StdioKind::Null => Some(File::open("NUL")?.into()),
        StdioKind::Piped => {
            let (read, write) = util::pipe()?;
            ends.stdin = Some(ChildStdin::from(write));
            Some(read.into())
        }
        StdioKind::Fd(handle) => Some(handle.try_clone()?),
    };
    stdio.error = match &command.stderr.0 {
        StdioKind::Inherit if err_console => None,
        StdioKind::Inherit => {
            let (read, write) = util::pipe()?;
            ends.stderr_tee = Some(read);
            Some(write)
        }
        StdioKind::Null => Some(File::options().write(true).open("NUL")?.into()),
        StdioKind::Piped => {
            let (read, write) = util::pipe()?;
            ends.stderr = Some(ChildStderr::from(OwnedHandle::from(read)));
            Some(write)
        }
        StdioKind::Fd(handle) => Some(handle.try_clone()?),
    };
    Ok((stdio, ends))
}
