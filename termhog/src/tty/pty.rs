//! Pseudoterminal plumbing: open a master/slave pair and make a std command's
//! child a session leader with the slave as its controlling terminal.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;

use rustix::fs::OFlags;
use rustix::io::ioctl_fionbio;
#[cfg(not(target_os = "linux"))]
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::process;
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{OptionalActions, Termios, Winsize, tcsetattr, tcsetwinsize};

/// An open pty pair. The master stays with us, the slave goes to the child.
pub struct Pty {
    pub master: File,
    pub slave: OwnedFd,
}

/// Open a pty of the given size with the host terminal's settings (`termios`),
/// the way script, tmux and ssh do, so line editing, flow control and
/// interrupt keys behave as they would natively. Both ends are close-on-exec
/// so concurrent spawns elsewhere in the process never inherit them.
///
/// The master is non-blocking, so writing input to a child that stopped
/// reading can never block shutdown. Its users wait for readiness instead.
pub fn open(size: (u16, u16), termios: &Termios) -> io::Result<Pty> {
    #[cfg(target_os = "linux")]
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    #[cfg(not(target_os = "linux"))]
    let master = {
        // No atomic close-on-exec flag here, so set it right away.
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        fcntl_setfd(&master, fcntl_getfd(&master)? | FdFlags::CLOEXEC)?;
        master
    };
    ioctl_fionbio(&master, true)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let name = ptsname(&master, Vec::new())?;
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlags::NOCTTY.bits() as i32)
        .open(OsStr::from_bytes(name.as_bytes()))?;
    tcsetattr(&slave, OptionalActions::Now, termios)?;
    let master = File::from(master);
    resize(&master, size)?;
    Ok(Pty {
        master,
        slave: slave.into(),
    })
}

/// Make the child a new session leader with `slave` as its controlling
/// terminal, the way a shell or terminal emulator starts a program. Without a
/// controlling terminal, Ctrl+C, job control, and `/dev/tty` wouldn't work.
pub fn make_controlling(cmd: &mut std::process::Command, slave: &OwnedFd) {
    let fd = slave.as_raw_fd();
    // SAFETY: the closure runs in the forked child and only makes
    // async-signal-safe syscalls, with no allocation. `fd` stays open until
    // exec (it's only close-on-exec).
    unsafe {
        cmd.pre_exec(move || {
            process::setsid()?;
            process::ioctl_tiocsctty(BorrowedFd::borrow_raw(fd))?;
            Ok(())
        });
    }
}

/// Resize the pty to `(cols, rows)`. The kernel sends the child SIGWINCH.
pub fn resize(master: &File, (cols, rows): (u16, u16)) -> io::Result<()> {
    let size = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    tcsetwinsize(master, size)?;
    Ok(())
}

/// On Linux a `read()` on the master after the child exits often returns `EIO`
/// instead of a clean 0-byte EOF. Treat it as EOF.
pub fn is_eof_error(e: &io::Error) -> bool {
    e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
}
