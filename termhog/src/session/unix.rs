//! Starting the child on Unix, exactly as std would, with a pty in place of
//! the streams the caller left on the terminal.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Instant;

use rustix::termios;

use super::Started;
use super::pump::Attachment;
use crate::tty::{self, pty};
use crate::util;
use crate::{Error, HogCommand, HogStdio, Result, TermHog};

/// Start `command`, on a pty when stdout is a terminal.
pub(super) fn start_child(settings: &TermHog, command: &HogCommand) -> Result<Started> {
    let out_tty = command.stdout.is_inherit() && termios::isatty(io::stdout());
    let err_tty = command.stderr.is_inherit() && termios::isatty(io::stderr());
    let in_tty = command.stdin.is_inherit() && termios::isatty(io::stdin());

    // Only take over the terminal when stdout is on it. When just stderr is
    // (e.g. `termhog -- make | less`), the terminal belongs to the next
    // program in the pipeline, so stderr is recorded through a pipe instead.
    let host = if out_tty {
        Attachment::open(settings.fallback_size)?
    } else {
        None
    };
    // When stdout is a terminal we can't take over (we're a background job,
    // or it isn't our controlling terminal), the child still gets a terminal
    // for its output, with the same settings and size, while its input stays
    // exactly as it is natively. If that can't be set up, it gets a pipe.
    let (attachment, pty, size) = match host {
        Some((attachment, pty)) => {
            let size = attachment.size;
            (Some(attachment), Some(pty), size)
        }
        None if out_tty => {
            let size = tty::winsize(io::stdout()).unwrap_or(settings.fallback_size);
            let termios = termios::tcgetattr(io::stdout()).ok();
            (None, termios.and_then(|t| pty::open(size, &t).ok()), size)
        }
        None => (None, None, settings.fallback_size),
    };

    // Streams the caller redirected go to std untouched. Inherited ones on
    // stdout's terminal are swapped for the pty (stdin only when input is
    // forwarded). Other inherited output goes through a pipe we tee, except
    // stderr on a different terminal than a pty-backed stdout, which keeps
    // its terminal.
    let slave = pty.as_ref().map(|p| &p.slave);
    let on_stdout_terminal = |fd: BorrowedFd| tty::same_terminal(fd, io::stdout());
    let in_pty = attachment.is_some() && in_tty && on_stdout_terminal(io::stdin().as_fd());
    let err_pty = slave.is_some() && err_tty && on_stdout_terminal(io::stderr().as_fd());
    let tee_stdout = slave.is_none() && command.stdout.is_inherit();
    let tee_stderr = command.stderr.is_inherit() && !(slave.is_some() && err_tty);
    // Output that all goes to one place (like `2>&1`) arrives there in the
    // order it's written, so it gets one pipe, read in order, not one each.
    let merged = if tee_stdout && tee_stderr && util::same_file(io::stdout(), io::stderr()) {
        Some(util::pipe()?)
    } else {
        None
    };
    let stdio = |on_pty: bool, tee: bool, spec: &HogStdio| -> io::Result<_> {
        Ok(match (slave, &merged) {
            (Some(slave), _) if on_pty => slave.try_clone()?.into(),
            (_, Some((_, write))) if tee => write.try_clone()?.into(),
            _ if tee => std::process::Stdio::piped(),
            _ => spec.to_std()?,
        })
    };
    let mut std_cmd = command.to_std();
    std_cmd.stdin(stdio(in_pty, false, &command.stdin)?);
    std_cmd.stdout(stdio(true, tee_stdout, &command.stdout)?);
    std_cmd.stderr(stdio(err_pty, tee_stderr, &command.stderr)?);
    // Only a child whose terminal we fully manage gets it as its controlling
    // terminal. Otherwise it keeps ours, with native job control.
    if let (Some(slave), Some(_)) = (slave, &attachment) {
        pty::make_controlling(&mut std_cmd, slave);
    }

    // Everything that can fail happened before the child exists, so an error
    // never leaves it running unattended.
    let mut child = match std_cmd.spawn() {
        Ok(child) => child,
        Err(source) => {
            if let Some(attachment) = attachment {
                attachment.abandon();
            }
            return Err(Error::Spawn {
                program: command.program.clone(),
                source,
            });
        }
    };
    let start = Instant::now();
    // Drop every slave and pipe write end we hold, so readers see EOF once
    // the child exits.
    drop(std_cmd);
    let mut tees = Vec::new();
    if let Some((read, _write)) = merged {
        tees.push((read, false));
    } else {
        if tee_stdout {
            let pipe = OwnedFd::from(child.stdout.take().expect("piped stdout"));
            tees.push((File::from(pipe), false));
        }
        if tee_stderr {
            let pipe = OwnedFd::from(child.stderr.take().expect("piped stderr"));
            tees.push((File::from(pipe), true));
        }
    }
    Ok(Started {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child,
        start,
        attachment,
        pty_output: pty.map(|pty| pty.master),
        tees,
        size,
    })
}
