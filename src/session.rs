//! Session orchestration: wires the pty, host terminal, and recorder across a
//! few threads over channels.
//!
//! Invariant: the host<->child byte tee is never gated by recording. If a
//! recording channel stalls, only recording suffers, never the terminal.

use std::ffi::{CStr, c_void};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, select, tick, unbounded};
use serde_json::Value;
use uuid::Uuid;

use crate::emulator::{AvtEmulator, Emulator};
use crate::projection::Projector;
use crate::terminal::ThemeColors;
use crate::util::{Utf8Decoder, epoch_ms};
use crate::{identity, pty, shipper, terminal};

/// Projection frame cadence: diff/emit at most this often, coalescing bursty
/// output to a human-perceptible rate.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// How often to emit a fresh FullSnapshot keyframe, so the player can seek and
/// mid-session joins render without replaying from the start.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(5);

const STDIN_FD: i32 = 0;
const STDOUT_FD: i32 = 1;
const STDERR_FD: i32 = 2;
const READ_BUF: usize = 32 * 1024;

/// What to record and how to run the child. Built by `main` from the CLI args
/// and environment, then handed to [`crate::run`].
pub struct Config {
    /// The child command and its arguments.
    pub command: Vec<String>,
    /// PostHog write-only project token (the public `phc_` token, not a secret).
    /// Required — the tool's purpose is to stream.
    pub api_key: String,
    /// Ingestion host, where events are POSTed (default US Cloud).
    pub ingest_host: String,
    /// Replay-UI host, where deep links live (differs from `ingest_host` only for
    /// proxied ingest).
    pub ui_host: String,
    /// Person ID. `None` => a stable anonymous ID persisted in the config dir.
    pub distinct_id: Option<String>,
    /// Session ID. `None` => a fresh UUIDv7 per run, so external events can share it.
    pub session_id: Option<String>,
    /// Where non-fatal upload diagnostics go. Defaults to no-op.
    pub reporter: Option<Arc<dyn crate::Reporter>>,
}

/// Result of a completed session.
pub struct SessionOutcome {
    /// Child exit status, mirrored (128+signum if killed by a signal).
    pub exit_status: i32,
    /// Replay deep link, when streaming was enabled.
    pub session_url: Option<String>,
}

/// A message to the emulator/projection thread.
enum EmuMsg {
    Output(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// The user typed. Mark the timeline active (no content).
    Input,
}

/// The recording pipeline: emulator/projection and shipper, plus what's needed
/// to build the replay link. Shared by the interactive (pty) and piped (non-tty)
/// capture paths, which differ only in how they spawn + tee IO.
struct Recorders {
    emu_tx: Sender<EmuMsg>,
    emu_handle: thread::JoinHandle<()>,
    ship_tx: Sender<shipper::Msg>,
    ship_handle: thread::JoinHandle<()>,
    replay: ReplayLink,
}

/// The pieces needed to build the replay deep link at teardown.
struct ReplayLink {
    ui_host: String,
    token: String,
    session_id: String,
}

/// Record and stream a session. Uses a pty for transparent passthrough on a real
/// terminal. On a non-tty (pipe/CI) it spawns with pipes so the child doesn't
/// mistake the pipe for an interactive terminal. Blocks until the child exits.
pub fn run(config: Config) -> Result<SessionOutcome> {
    let interactive = terminal::stdin_is_tty() && terminal::stdout_is_tty();
    let (cols, rows) = initial_size(interactive);

    // Belt-and-suspenders terminal restoration for panics on any thread. If
    // anything below fails, the raw-mode guard drops and restores the terminal.
    terminal::install_panic_hook();
    // Raw mode + theme capture are only meaningful (and possible) on a real tty.
    let _raw_guard = if interactive {
        Some(terminal::RawModeGuard::enter().context("entering raw mode")?)
    } else {
        None
    };
    // Probe the host terminal's colors and cell pixel size in one round-trip,
    // before the child runs and before the stdin-forward thread owns stdin.
    let term = if interactive {
        terminal::query_terminal(cols, rows)
    } else {
        terminal::TerminalInfo::default()
    };

    let start = Instant::now();

    // Identity + session, resolved before spawn. distinct_id (person) is stable.
    // session_id is fresh per run unless pinned via config/env.
    let distinct_id = identity::resolve(config.distinct_id.clone());
    let session_id = config
        .session_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());

    let recorders = setup_recorders(&config, cols, rows, term, distinct_id, session_id)?;

    let exit_status = if interactive {
        run_interactive(&config, cols, rows, &recorders)?
    } else {
        run_piped(&config, &recorders)?
    };

    let session_url = teardown_recorders(recorders, exit_status, start);
    // _raw_guard drops on return, restoring the host terminal.

    Ok(SessionOutcome {
        exit_status,
        session_url,
    })
}

/// Spawn the shipper and emulator/projection threads.
fn setup_recorders(
    config: &Config,
    cols: u16,
    rows: u16,
    term: terminal::TerminalInfo,
    distinct_id: String,
    session_id: String,
) -> Result<Recorders> {
    let terminal::TerminalInfo { theme, cell } = term;

    let api_key = config.api_key.clone();
    let ui_host = config.ui_host.clone();
    let cfg = shipper::Config {
        ingest_host: config.ingest_host.clone(),
        api_key: api_key.clone(),
        distinct_id,
        session_id: session_id.clone(),
        command: config.command.join(" "),
        lib_version: env!("CARGO_PKG_VERSION").to_string(),
        reporter: config.reporter.clone(),
    };
    let (ship_tx, ship_rx) = unbounded::<shipper::Msg>();
    let ship_handle = thread::spawn(move || shipper::run(cfg, ship_rx));

    let (emu_tx, emu_rx) = unbounded::<EmuMsg>();
    let emu_ship_tx = ship_tx.clone();
    let emu_handle =
        thread::spawn(move || emulator_loop(emu_rx, cols, rows, theme, cell, emu_ship_tx));

    Ok(Recorders {
        emu_tx,
        emu_handle,
        ship_tx,
        ship_handle,
        replay: ReplayLink {
            ui_host,
            token: api_key,
            session_id,
        },
    })
}

/// Flush and join the recorders after the IO threads have drained. Returns the
/// replay deep link (with a `?t=` seek near the end on non-zero exit).
fn teardown_recorders(rec: Recorders, exit_status: i32, start: Instant) -> Option<String> {
    // Emulator: close so it flushes a final frame + remaining events to the shipper.
    drop(rec.emu_tx);
    let _ = rec.emu_handle.join();
    // Shipper: flush remaining + term_end, then join.
    let duration_ms = start.elapsed().as_millis() as u64;
    let _ = rec.ship_tx.send(shipper::Msg::Terminate {
        exit_code: exit_status,
        duration_ms,
    });
    drop(rec.ship_tx);
    let _ = rec.ship_handle.join();

    let seek = (exit_status != 0).then(|| start.elapsed().as_secs().saturating_sub(10));
    Some(shipper::replay_url(
        &rec.replay.ui_host,
        &rec.replay.token,
        &rec.replay.session_id,
        seek,
    ))
}

/// Interactive path: run the child under a pty for transparent passthrough.
fn run_interactive(config: &Config, cols: u16, rows: u16, rec: &Recorders) -> Result<i32> {
    let proc = pty::spawn(&config.command, cols, rows)?;
    let pty::PtyProcess {
        master,
        mut child,
        reader,
        writer,
        pgid,
    } = proc;

    // Read thread: pty -> stdout (hot path) + emulator.
    let read_handle = {
        let emu_tx = rec.emu_tx.clone();
        thread::spawn(move || read_loop(reader, emu_tx))
    };

    // Stdin-forward thread: host stdin -> pty. A shutdown flag lets it stop after
    // the child exits (a blocking read on stdin can't otherwise be interrupted).
    let shutdown = Arc::new(AtomicBool::new(false));
    let stdin_handle = {
        let shutdown = Arc::clone(&shutdown);
        let emu_tx = rec.emu_tx.clone();
        thread::spawn(move || stdin_loop(writer, shutdown, emu_tx))
    };

    let (sig_handle, sig_join) = spawn_signal_thread(Arc::clone(&master), pgid, rec.emu_tx.clone());

    let status = child.wait().context("waiting for child")?;
    let exit_status = resolved_exit_code(&status);

    // Drain output, then stop the input/signal threads.
    let _ = read_handle.join();
    shutdown.store(true, Ordering::SeqCst);
    let _ = stdin_handle.join();
    sig_handle.close();
    let _ = sig_join.join();
    drop(master); // kept alive until the read loop could reach EOF
    Ok(exit_status)
}

/// Non-tty path: spawn with pipes so the child sees a pipe (not a fake tty), and
/// tee stdout/stderr to the real fds + the recorders. No raw mode, pty, resize,
/// or stdin forwarding — stdin is inherited directly.
fn run_piped(config: &Config, rec: &Recorders) -> Result<i32> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(&config.command[0]);
    cmd.args(&config.command[1..]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", config.command[0]))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_handle = {
        let emu_tx = rec.emu_tx.clone();
        thread::spawn(move || pipe_tee(stdout, STDOUT_FD, emu_tx))
    };
    let err_handle = {
        let emu_tx = rec.emu_tx.clone();
        thread::spawn(move || pipe_tee(stderr, STDERR_FD, emu_tx))
    };

    let status = child.wait().context("waiting for child")?;
    let _ = out_handle.join();
    let _ = err_handle.join();
    Ok(std_exit_code(status))
}

/// Tee a child pipe to a real fd (byte-exact passthrough) and into the emulator
/// channel. stdout and stderr both feed the single emulator stream in arrival
/// order, mirroring how a terminal interleaves them.
fn pipe_tee(mut reader: impl Read, fd: i32, emu_tx: Sender<EmuMsg>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if write_all_fd(fd, &buf[..n]).is_err() {
                    break;
                }
                let _ = emu_tx.send(EmuMsg::Output(buf[..n].to_vec()));
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Exit code from a `std::process::ExitStatus`, mirroring `128 + signum` when the
/// child was killed by a signal.
fn std_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

/// Initial terminal geometry: from the host tty when interactive, else from
/// COLUMNS/LINES, else a sane default.
fn initial_size(interactive: bool) -> (u16, u16) {
    if interactive {
        if let Ok(size) = terminal::host_winsize() {
            return size;
        }
    }
    (env_dim("COLUMNS", 80), env_dim("LINES", 24))
}

fn env_dim(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Feed the emulator, project on a frame tick, and emit rrweb events to the
/// shipper for live streaming.
fn emulator_loop(
    rx: Receiver<EmuMsg>,
    cols: u16,
    rows: u16,
    theme: Option<ThemeColors>,
    cell: Option<terminal::CellSize>,
    ship_tx: Sender<shipper::Msg>,
) {
    let mut emu = AvtEmulator::new(cols, rows);
    let mut proj = Projector::new(cols, rows, theme, cell);
    let mut decoder = Utf8Decoder::default();
    let emit = |ev: Value| {
        let _ = ship_tx.send(shipper::Msg::Event(ev));
    };

    // Baseline: Meta, initial FullSnapshot, then hide the mouse cursor.
    emit(proj.meta(epoch_ms()));
    emit(proj.full_snapshot(&emu, epoch_ms()));
    emit(proj.hide_mouse(epoch_ms()));

    let ticker = tick(FRAME_INTERVAL);
    let mut last_keyframe = Instant::now();
    let mut input_pending = false;

    loop {
        select! {
            recv(rx) -> msg => match msg {
                Ok(EmuMsg::Output(bytes)) => {
                    let text = decoder.push(&bytes);
                    if !text.is_empty() {
                        emu.feed_str(&text);
                    }
                }
                Ok(EmuMsg::Resize { cols, rows }) => {
                    emu.resize(cols, rows);
                    proj.resize(cols, rows);
                    // Geometry changed: fresh Meta + keyframe, never diff across it.
                    emit(proj.meta(epoch_ms()));
                    emit(proj.full_snapshot(&emu, epoch_ms()));
                    last_keyframe = Instant::now();
                }
                Ok(EmuMsg::Input) => input_pending = true,
                Err(_) => break, // channel closed at teardown
            },
            recv(ticker) -> _ => {
                // Coalesce activity to the frame tick to bound event rate.
                if input_pending {
                    emit(proj.activity(epoch_ms()));
                    input_pending = false;
                }
                if last_keyframe.elapsed() >= KEYFRAME_INTERVAL {
                    emit(proj.full_snapshot(&emu, epoch_ms()));
                    last_keyframe = Instant::now();
                } else if let Some(mutation) = proj.diff(&emu, epoch_ms()) {
                    emit(mutation);
                }
            },
        }
    }

    // Drain any buffered bytes and capture a final frame.
    let leftover = decoder.flush();
    if !leftover.is_empty() {
        emu.feed_str(&leftover);
    }
    if let Some(mutation) = proj.diff(&emu, epoch_ms()) {
        emit(mutation);
    }
}

/// pty -> stdout (hot path) + emulator channel.
fn read_loop(mut reader: Box<dyn Read + Send>, emu_tx: Sender<EmuMsg>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // clean EOF
            Ok(n) => {
                // Hot path first: never let recording delay the terminal.
                if let Err(e) = write_all_fd(STDOUT_FD, &buf[..n]) {
                    log::warn!("stdout write error: {e}");
                    break;
                }
                let _ = emu_tx.send(EmuMsg::Output(buf[..n].to_vec()));
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if pty::is_eof_error(e) => break, // child gone (Linux EIO)
            Err(e) => {
                log::warn!("pty read error: {e}");
                break;
            }
        }
    }
}

/// host stdin -> pty. Polls stdin so a set shutdown flag ends the loop even
/// though the child has exited and no more input will arrive.
///
/// When host stdin closes (or errors), we do NOT return: dropping the pty writer
/// makes portable-pty inject `\n`+EOF into the master (see UnixMasterWriter::drop),
/// and the slave echoes that newline back into the recording mid-session. Instead
/// we hold the writer until teardown, so it is dropped only after the child has
/// exited and nothing is reading the master.
fn stdin_loop(
    mut writer: Box<dyn Write + Send>,
    shutdown: Arc<AtomicBool>,
    emu_tx: Sender<EmuMsg>,
) {
    let mut buf = [0u8; READ_BUF];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match poll_readable(STDIN_FD, 100) {
            PollResult::Ready => {}
            PollResult::TimedOut => continue,
            PollResult::Error => {
                park_until_shutdown(&shutdown);
                break;
            }
        }
        match read_fd(STDIN_FD, &mut buf) {
            Ok(0) => {
                park_until_shutdown(&shutdown);
                break;
            }
            Ok(n) => {
                if writer
                    .write_all(&buf[..n])
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    park_until_shutdown(&shutdown);
                    break;
                }
                // Mark activity (no content) so typing counts as active time.
                let _ = emu_tx.send(EmuMsg::Input);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                park_until_shutdown(&shutdown);
                break;
            }
        }
    }
}

/// Block until the shutdown flag is set, so the caller can keep resources alive
/// until teardown instead of dropping them mid-session.
fn park_until_shutdown(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Spawn the signal-handling thread. Returns the signal-hook handle (to stop it)
/// and the join handle.
fn spawn_signal_thread(
    master: Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pgid: Option<i32>,
    emu_tx: Sender<EmuMsg>,
) -> (signal_hook::iterator::Handle, thread::JoinHandle<()>) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};
    use signal_hook::iterator::Signals;

    let mut signals =
        Signals::new([SIGWINCH, SIGTERM, SIGHUP, SIGINT, SIGQUIT]).expect("register signals");
    let handle = signals.handle();
    let join = thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGWINCH => {
                    if let Ok((cols, rows)) = terminal::host_winsize() {
                        let _ = pty::resize(&master, cols, rows);
                        let _ = emu_tx.send(EmuMsg::Resize { cols, rows });
                    }
                }
                other => {
                    // Forward terminating signals to the child's process group,
                    // mirroring native behavior. The child dying ends child.wait,
                    // and main then closes this handle to stop the loop.
                    if let Some(pg) = pgid {
                        unsafe {
                            libc::killpg(pg, other);
                        }
                    }
                }
            }
        }
    });
    (handle, join)
}

enum PollResult {
    Ready,
    TimedOut,
    Error,
}

/// Poll a single fd for readability with a millisecond timeout.
fn poll_readable(fd: i32, timeout_ms: i32) -> PollResult {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    match r {
        0 => PollResult::TimedOut,
        n if n > 0 => PollResult::Ready,
        _ => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                PollResult::TimedOut
            } else {
                PollResult::Error
            }
        }
    }
}

/// Read from a raw fd. Retrying on EINTR is handled by the caller.
fn read_fd(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Write all bytes to a raw fd, looping over partial writes. Bypasses
/// `std::io::Stdout` line buffering, which would corrupt raw passthrough.
fn write_all_fd(fd: i32, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// Mirror the child's exit status using the shell convention: `128 + signum`
/// when killed by a signal. portable-pty gives us the signal *name* (via
/// `strsignal`). We reverse it back to a number by matching against the same
/// `strsignal` table in this process/locale.
fn resolved_exit_code(status: &portable_pty::ExitStatus) -> i32 {
    match status.signal() {
        Some(name) => 128 + signal_number_from_name(name).unwrap_or(1),
        None => status.exit_code() as i32,
    }
}

fn signal_number_from_name(name: &str) -> Option<i32> {
    // portable-pty formats unknown signals as "Signal {n}".
    if let Some(rest) = name.strip_prefix("Signal ") {
        if let Ok(n) = rest.trim().parse::<i32>() {
            return Some(n);
        }
    }
    for sig in 1..=31 {
        let ptr = unsafe { libc::strsignal(sig) };
        if ptr.is_null() {
            continue;
        }
        let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
        if s == name {
            return Some(sig);
        }
    }
    None
}
