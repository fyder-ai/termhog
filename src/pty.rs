//! PTY capture layer: open a pty, spawn the child transparently, and expose the
//! reader/writer/master the session threads need.

use std::io;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// A spawned child running under a pty, decomposed into the handles the session
/// threads own independently.
pub struct PtyProcess {
    /// Shared so the signal thread can `resize()` while the main thread keeps it
    /// alive. Kept alive until teardown so the read loop can reach EOF.
    pub master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub child: Box<dyn Child + Send + Sync>,
    /// Blocking reader over the master; owned by the read thread.
    pub reader: Box<dyn io::Read + Send>,
    /// Writer to the master; owned by the stdin-forward thread (take_writer can
    /// only be called once).
    pub writer: Box<dyn io::Write + Send>,
    /// Child's process-group leader pid, for forwarding terminating signals.
    pub pgid: Option<i32>,
}

/// Open a pty of the given size, spawn `argv` in it inheriting cwd + environment
/// (with `TERM` ensured), and return the decomposed handles. `extra_env` is set
/// on the child on top of the inherited environment.
pub fn spawn(
    argv: &[String],
    cols: u16,
    rows: u16,
    extra_env: &[(&str, String)],
) -> Result<PtyProcess> {
    if argv.is_empty() {
        bail!("no command to run");
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening pty")?;

    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Inherit cwd for transparent passthrough. The full environment is inherited
    // by default; ensure TERM is present so the child emits the right sequences.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    // So the wrapped command's own PostHog SDK can tag its events/exceptions to
    // this replay's session and person.
    for (key, val) in extra_env {
        cmd.env(key, val);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {}", argv[0]))?;
    // Critical: drop the slave so the master sees EOF once the child exits.
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().context("cloning pty reader")?;
    let writer = pair.master.take_writer().context("taking pty writer")?;
    let pgid = pair.master.process_group_leader();

    Ok(PtyProcess {
        master: Arc::new(Mutex::new(pair.master)),
        child,
        reader,
        writer,
        pgid,
    })
}

/// Resize the pty (updates the kernel winsize and signals SIGWINCH to the child).
pub fn resize(master: &Arc<Mutex<Box<dyn MasterPty + Send>>>, cols: u16, rows: u16) -> Result<()> {
    let master = master.lock().unwrap();
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("resizing pty")
}

/// On Linux a `read()` on the master after the child exits often returns `EIO`
/// instead of a clean 0-byte EOF. Treat it as EOF.
pub fn is_eof_error(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EIO)
}
