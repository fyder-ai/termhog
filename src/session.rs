//! Session orchestration: wires the pty, host terminal, and recorder across a
//! few threads over channels.
//!
//! Invariant: the host<->child byte tee is never gated by recording. If a
//! recording channel or disk stalls, only recording suffers, never the terminal.

use std::ffi::{CStr, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, select, tick, unbounded};
use serde_json::Value;
use uuid::Uuid;

use crate::cast::{CastWriter, Header, Theme};
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

/// What to record and how to run the child.
pub struct Config {
    /// The child command and its arguments (everything after `--`).
    pub command: Vec<String>,
    /// Optional asciicast output path, for debugging. Not a product output.
    pub cast_path: Option<PathBuf>,
    /// Optional path to dump the projected rrweb event array as JSON, for the
    /// local rrweb-player harness. Debugging only, not a product output.
    pub rrweb_debug_path: Option<PathBuf>,
    /// Record user keystrokes as `i` events. Off by default; passwords must not
    /// leak.
    pub record_input: bool,
    /// PostHog write-only project token (the public `phc_` token, not a secret).
    /// Required — the tool's purpose is to stream. The CLI errors if it's unset.
    pub api_key: String,
    /// Ingestion host, e.g. `https://us.i.posthog.com`. The app host for deep
    /// links is derived from it (Cloud `*.i.` -> `*.`).
    pub posthog_host: String,
    /// Person id. Defaults to a stable id persisted in the config dir.
    pub distinct_id: Option<String>,
    /// Session id. Defaults to a fresh UUIDv7 per run; set it to pin the session
    /// (e.g. from `PH_CAPTURE_SESSION_ID`) so external events share it.
    pub session_id: Option<String>,
    /// Where non-fatal upload diagnostics go. Defaults to no-op.
    pub reporter: Option<Arc<dyn crate::Reporter>>,
}

/// Result of a completed session.
pub struct SessionOutcome {
    /// Child exit status, mirrored (128+signum if killed by a signal).
    pub exit_status: i32,
    pub cast_path: Option<PathBuf>,
    /// Replay deep link, when streaming was enabled.
    pub session_url: Option<String>,
}

/// A timestamped message to the cast-writer thread.
enum CastMsg {
    Output { at: Instant, data: Vec<u8> },
    Input { at: Instant, data: Vec<u8> },
    Resize { at: Instant, cols: u16, rows: u16 },
    Exit { at: Instant, status: i32 },
}

/// A message to the emulator/projection thread.
enum EmuMsg {
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    /// The user typed; mark the timeline active (no content).
    Input,
}

/// The recording pipeline: cast writer, emulator/projection, and shipper, plus
/// what's needed to build the replay link. Shared by the interactive (pty) and
/// piped (non-tty) capture paths, which differ only in how they spawn + tee IO.
struct Recorders {
    cast_tx: Option<Sender<CastMsg>>,
    cast_handle: Option<thread::JoinHandle<()>>,
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
/// terminal; on a non-tty (pipe/CI) it spawns with pipes so the child doesn't
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

    // Identity + session, resolved before spawn so we can hand them to the child.
    // distinct_id (person) is stable; session_id is fresh per run unless pinned.
    let distinct_id = identity::resolve(config.distinct_id.clone());
    let session_id = config
        .session_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    // Inject session + person into the child so its own PostHog SDK can tag
    // events/exceptions to this replay.
    let child_env = [
        ("PH_CAPTURE_SESSION_ID", session_id.clone()),
        ("PH_CAPTURE_DISTINCT_ID", distinct_id.clone()),
    ];

    let recorders = setup_recorders(&config, cols, rows, start, term, distinct_id, session_id)?;

    let exit_status = if interactive {
        run_interactive(&config, cols, rows, &recorders, &child_env)?
    } else {
        run_piped(&config, &recorders, &child_env)?
    };

    let session_url = teardown_recorders(recorders, exit_status, start);
    // _raw_guard drops on return, restoring the host terminal.

    Ok(SessionOutcome {
        exit_status,
        cast_path: config.cast_path,
        session_url,
    })
}

/// Spawn the cast writer, shipper, and emulator/projection threads.
fn setup_recorders(
    config: &Config,
    cols: u16,
    rows: u16,
    start: Instant,
    term: terminal::TerminalInfo,
    distinct_id: String,
    session_id: String,
) -> Result<Recorders> {
    let terminal::TerminalInfo { theme, cell } = term;
    let (cast_tx, cast_handle) = match &config.cast_path {
        Some(path) => {
            let (tx, handle) =
                spawn_cast_writer(path, cols, rows, start, &config.command, theme.clone())
                    .with_context(|| format!("opening cast file {}", path.display()))?;
            (Some(tx), Some(handle))
        }
        None => (None, None),
    };

    let api_key = config.api_key.clone();
    let ui_host = shipper::derive_ui_host(&config.posthog_host);
    let cfg = shipper::Config {
        ingest_host: config.posthog_host.clone(),
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
    let out_path = config.rrweb_debug_path.clone();
    let ship = Some(ship_tx.clone());
    let emu_handle =
        thread::spawn(move || emulator_loop(emu_rx, cols, rows, theme, cell, out_path, ship));

    Ok(Recorders {
        cast_tx,
        cast_handle,
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
    // Cast: record the exit event, then close + flush.
    if let Some(tx) = &rec.cast_tx {
        let _ = tx.send(CastMsg::Exit {
            at: Instant::now(),
            status: exit_status,
        });
    }
    drop(rec.cast_tx);
    if let Some(handle) = rec.cast_handle {
        let _ = handle.join();
    }
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
fn run_interactive(
    config: &Config,
    cols: u16,
    rows: u16,
    rec: &Recorders,
    child_env: &[(&str, String)],
) -> Result<i32> {
    let proc = pty::spawn(&config.command, cols, rows, child_env)?;
    let pty::PtyProcess {
        master,
        mut child,
        reader,
        writer,
        pgid,
    } = proc;

    // Read thread: pty -> stdout (hot path) + cast + emulator.
    let read_handle = {
        let cast_tx = rec.cast_tx.clone();
        let emu_tx = Some(rec.emu_tx.clone());
        thread::spawn(move || read_loop(reader, cast_tx, emu_tx))
    };

    // Stdin-forward thread: host stdin -> pty. A shutdown flag lets it stop after
    // the child exits (a blocking read on stdin can't otherwise be interrupted).
    let shutdown = Arc::new(AtomicBool::new(false));
    let stdin_handle = {
        let shutdown = Arc::clone(&shutdown);
        let cast_tx = if config.record_input { rec.cast_tx.clone() } else { None };
        let emu_tx = Some(rec.emu_tx.clone());
        let master = Arc::clone(&master);
        thread::spawn(move || stdin_loop(writer, shutdown, cast_tx, emu_tx, master))
    };

    let (sig_handle, sig_join) = spawn_signal_thread(
        Arc::clone(&master),
        pgid,
        rec.cast_tx.clone(),
        Some(rec.emu_tx.clone()),
    );

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
fn run_piped(config: &Config, rec: &Recorders, child_env: &[(&str, String)]) -> Result<i32> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(&config.command[0]);
    cmd.args(&config.command[1..]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    for (key, val) in child_env {
        cmd.env(key, val);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", config.command[0]))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_handle = {
        let cast_tx = rec.cast_tx.clone();
        let emu_tx = Some(rec.emu_tx.clone());
        thread::spawn(move || pipe_tee(stdout, STDOUT_FD, cast_tx, emu_tx))
    };
    let err_handle = {
        let cast_tx = rec.cast_tx.clone();
        let emu_tx = Some(rec.emu_tx.clone());
        thread::spawn(move || pipe_tee(stderr, STDERR_FD, cast_tx, emu_tx))
    };

    let status = child.wait().context("waiting for child")?;
    let _ = out_handle.join();
    let _ = err_handle.join();
    Ok(std_exit_code(status))
}

/// Fan one output chunk out to the cast + emulator recording channels. Each sink
/// takes an owned copy. The byte tee to the terminal happens on the caller's hot
/// path before this, so recording never delays passthrough.
fn send_output(at: Instant, data: &[u8], cast_tx: &Option<Sender<CastMsg>>, emu_tx: &Option<Sender<EmuMsg>>) {
    if let Some(tx) = cast_tx {
        let _ = tx.send(CastMsg::Output {
            at,
            data: data.to_vec(),
        });
    }
    if let Some(tx) = emu_tx {
        let _ = tx.send(EmuMsg::Output(data.to_vec()));
    }
}

/// Tee a child pipe to a real fd (byte-exact passthrough) and into the cast +
/// emulator channels. stdout and stderr both feed the single emulator stream in
/// arrival order, mirroring how a terminal interleaves them.
fn pipe_tee(
    mut reader: impl Read,
    fd: i32,
    cast_tx: Option<Sender<CastMsg>>,
    emu_tx: Option<Sender<EmuMsg>>,
) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let at = Instant::now();
                if write_all_fd(fd, &buf[..n]).is_err() {
                    break;
                }
                send_output(at, &buf[..n], &cast_tx, &emu_tx);
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

/// Create the cast file, spawn the writer thread, return its channel + handle.
fn spawn_cast_writer(
    path: &PathBuf,
    cols: u16,
    rows: u16,
    start: Instant,
    command: &[String],
    theme: Option<terminal::ThemeColors>,
) -> Result<(Sender<CastMsg>, thread::JoinHandle<()>)> {
    let file = File::create(path)?;
    let mut header = Header::new(cols, rows);
    header.command = Some(command.join(" "));
    header.term.theme = theme.map(|t| Theme {
        fg: t.fg,
        bg: t.bg,
        palette: t.palette.join(":"),
    });
    for key in ["SHELL", "TERM", "LANG"] {
        if let Ok(val) = std::env::var(key) {
            header.env.insert(key.to_string(), val);
        }
    }
    let cast = CastWriter::new(BufWriter::new(file), start, &header)?;
    let (tx, rx) = unbounded::<CastMsg>();
    let handle = thread::spawn(move || cast_writer_loop(cast, rx));
    Ok((tx, handle))
}

/// Drain the cast channel, flushing periodically so a crash loses at most a
/// fraction of a second.
fn cast_writer_loop(mut cast: CastWriter<BufWriter<File>>, rx: Receiver<CastMsg>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(CastMsg::Output { at, data }) => {
                if let Err(e) = cast.output(at, &data) {
                    log::warn!("cast write error: {e}");
                    break;
                }
            }
            Ok(CastMsg::Input { at, data }) => {
                let _ = cast.input(at, &data);
            }
            Ok(CastMsg::Resize { at, cols, rows }) => {
                let _ = cast.resize(at, cols, rows);
            }
            Ok(CastMsg::Exit { at, status }) => {
                let _ = cast.exit(at, status);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = cast.flush();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = cast.flush();
}

/// Feed the emulator, project on a frame tick, and emit rrweb events to the
/// shipper (live streaming) and/or a JSON file (local rrweb-player harness).
fn emulator_loop(
    rx: Receiver<EmuMsg>,
    cols: u16,
    rows: u16,
    theme: Option<ThemeColors>,
    cell: Option<terminal::CellSize>,
    out_path: Option<PathBuf>,
    ship_tx: Option<Sender<shipper::Msg>>,
) {
    let mut emu = AvtEmulator::new(cols, rows);
    let mut proj = Projector::new(cols, rows, theme, cell);
    let mut decoder = Utf8Decoder::default();
    // Only retain events in memory when writing the debug file.
    let mut events: Vec<Value> = Vec::new();
    let collect = out_path.is_some();
    let mut emit = |ev: Value| {
        if let Some(tx) = &ship_tx {
            let _ = tx.send(shipper::Msg::Event(ev.clone()));
        }
        if collect {
            events.push(ev);
        }
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

    if let Some(path) = out_path {
        match File::create(&path) {
            Ok(file) => {
                if let Err(e) = serde_json::to_writer(BufWriter::new(file), &events) {
                    log::warn!("failed writing rrweb debug output: {e}");
                }
            }
            Err(e) => log::warn!("failed creating rrweb debug output {}: {e}", path.display()),
        }
    }
}

/// pty -> stdout (hot path) + cast + emulator channels. Timestamps each chunk.
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    cast_tx: Option<Sender<CastMsg>>,
    emu_tx: Option<Sender<EmuMsg>>,
) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // clean EOF
            Ok(n) => {
                let at = Instant::now();
                // Hot path first: never let recording delay the terminal.
                if let Err(e) = write_all_fd(STDOUT_FD, &buf[..n]) {
                    log::warn!("stdout write error: {e}");
                    break;
                }
                send_output(at, &buf[..n], &cast_tx, &emu_tx);
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
    cast_tx: Option<Sender<CastMsg>>,
    emu_tx: Option<Sender<EmuMsg>>,
    master: Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
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
                if writer.write_all(&buf[..n]).and_then(|_| writer.flush()).is_err() {
                    park_until_shutdown(&shutdown);
                    break;
                }
                // Mark activity (no content) so typing counts as active time.
                if let Some(tx) = &emu_tx {
                    let _ = tx.send(EmuMsg::Input);
                }
                // Record input only when the child echoes it — matching the
                // child means secrets at echo-off prompts aren't recorded.
                if let Some(tx) = &cast_tx {
                    if child_echo_on(&master) {
                        let _ = tx.send(CastMsg::Input {
                            at: Instant::now(),
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                park_until_shutdown(&shutdown);
                break;
            }
        }
    }
}

/// Whether the child pty currently echoes input. Gates input recording: echo
/// off (e.g. a password prompt) means don't record. Unknown => assume on.
fn child_echo_on(master: &Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>) -> bool {
    master
        .lock()
        .ok()
        .and_then(|m| m.get_termios())
        .map(|t| t.local_flags.contains(nix::sys::termios::LocalFlags::ECHO))
        .unwrap_or(true)
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
    cast_tx: Option<Sender<CastMsg>>,
    emu_tx: Option<Sender<EmuMsg>>,
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
                        if let Some(tx) = &cast_tx {
                            let _ = tx.send(CastMsg::Resize {
                                at: Instant::now(),
                                cols,
                                rows,
                            });
                        }
                        if let Some(tx) = &emu_tx {
                            let _ = tx.send(EmuMsg::Resize { cols, rows });
                        }
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

/// Read from a raw fd, retrying on EINTR is handled by the caller.
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
/// `strsignal`); we reverse it back to a number by matching against the same
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
