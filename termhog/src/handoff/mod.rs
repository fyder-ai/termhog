//! Finishing recordings in the background, so a slow network (or a burst of
//! output still being rendered) never holds up the app's exit.
//!
//! When a recording ends with work left, that work is saved as a checkpoint
//! (see [`store`]) and the uploader is launched: the app's own executable,
//! started again detached, in its own session, with no terminal and no
//! output. Its [`crate::init`] call notices it's the uploader, finishes every
//! checkpoint waiting (rendering what's left, then uploading with retries),
//! and exits without running the rest of the app. Checkpoints it can't finish
//! (the machine shuts down, the network stays down) are picked up by the next
//! `init` that finds them.
//!
//! In CI there's no prompt to return to, and leftover processes are killed
//! (loudly) when the job ends, so recordings are finished in-process there,
//! with a longer wait.

mod checkpoint;
mod store;

use std::collections::HashSet;
use std::io::BufReader;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::render::Remainder;
use crate::upload::{self, Config, Leftovers};

/// Set in the environment of a launched uploader.
const UPLOADER_VAR: &str = "TERMHOG_UPLOADER";
/// How long an uploader keeps retrying before leaving the rest for later.
const UPLOADER_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// How long a recording gets to finish in-process before being handed off.
const FINISH_TIMEOUT: Duration = Duration::from_millis(300);
/// The same, in CI, where the process can't be left to finish later.
const FINISH_TIMEOUT_CI: Duration = Duration::from_secs(20);

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// See [`crate::init`].
pub fn init() {
    if std::env::var_os(UPLOADER_VAR).is_some() {
        run_uploader();
        std::process::exit(0);
    }
    INITIALIZED.store(true, Ordering::SeqCst);
    if store::any_unclaimed() {
        launch_uploader();
    }
}

/// Panic unless [`crate::init`] ran, so recordings can always be finished.
pub fn ensure_initialized() {
    assert!(
        INITIALIZED.load(Ordering::SeqCst),
        "termhog::init() must be called at the top of main before recording"
    );
}

/// How long a recording gets to finish in-process before being handed off,
/// unless the caller chose.
pub fn default_finish_timeout() -> Duration {
    if in_ci() {
        FINISH_TIMEOUT_CI
    } else {
        FINISH_TIMEOUT
    }
}

/// Whether this looks like a CI job (or a container's main process), where
/// nothing survives the job for long.
fn in_ci() -> bool {
    const VARS: [&str; 10] = [
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "TF_BUILD",
        "JENKINS_URL",
        "CIRCLECI",
        "TEAMCITY_VERSION",
        "BITBUCKET_BUILD_NUMBER",
        "CODEBUILD_BUILD_ID",
    ];
    static IN_CI: OnceLock<bool> = OnceLock::new();
    *IN_CI.get_or_init(|| {
        VARS.iter()
            .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
            || std::process::id() == 1
    })
}

/// Save a recording's unfinished work and have the background uploader
/// finish it.
pub fn hand_off(config: &Config, mut leftovers: Leftovers, render: Option<Remainder>) {
    let saved = store::save(|file| checkpoint::write(file, config, &mut leftovers, render));
    if saved.is_ok() {
        launch_uploader();
    }
}

/// Start the background uploader: this executable again, detached from the
/// terminal and from this process, with nothing inherited but the
/// environment. Not in CI, where the job's end would kill it: checkpoints
/// saved there wait for a later run on the same machine.
fn launch_uploader() {
    if in_ci() {
        return;
    }
    let Some(exe) = own_executable() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        // What `ps` shows, so the process explains itself.
        .arg0("termhog-uploader")
        .env(UPLOADER_VAR, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = store::dir() {
        // Not the app's directory, which it may want to unmount or delete.
        command.current_dir(dir);
    }
    // SAFETY: setsid is async-signal-safe and allocates nothing. A session of
    // its own leaves the terminal behind, so closing it doesn't end uploads.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }
    if let Ok(mut child) = command.spawn() {
        // Reap it if it finishes while this process still runs.
        thread::spawn(move || child.wait());
    }
}

/// The running executable. If its file was deleted since it started (say, by
/// an upgrade), Linux still reaches it through the kernel's own reference.
fn own_executable() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok().filter(|path| path.exists());
    if cfg!(target_os = "linux") {
        exe.or_else(|| Some(PathBuf::from("/proc/self/exe")))
    } else {
        exe
    }
}

/// The uploader: finish every checkpoint waiting, until none are left or its
/// lifetime runs out.
fn run_uploader() {
    close_inherited_files();
    let give_up_at = Instant::now() + UPLOADER_LIFETIME;
    // New checkpoints may arrive meanwhile, so look again after each round,
    // skipping ones already tried (a damaged one would fail every time).
    let mut tried = HashSet::new();
    loop {
        let claimed: Vec<_> = store::claim_all()
            .into_iter()
            .filter(|c| tried.insert(c.path.clone()))
            .collect();
        if claimed.is_empty() {
            return;
        }
        for checkpoint in claimed {
            finish(checkpoint, give_up_at);
            if Instant::now() >= give_up_at {
                return;
            }
        }
    }
}

/// Finish one checkpoint: render what's left, then upload. What can't be
/// uploaded before `give_up_at` is saved back for later.
fn finish(mut claimed: store::Claimed, give_up_at: Instant) {
    // A checkpoint of a newer format is left for a newer version, and a
    // damaged one to expire.
    let Some(checkpoint::Loaded {
        config,
        mut leftovers,
        rendered,
    }) = checkpoint::load(BufReader::new(&claimed.file))
    else {
        return;
    };
    // Save the rendered events, so rendering never has to be redone.
    let save = |claimed: &mut store::Claimed, leftovers: &mut Leftovers| {
        store::replace(claimed, |f| checkpoint::write(f, &config, leftovers, None))
    };
    if rendered && save(&mut claimed, &mut leftovers).is_err() {
        return;
    }
    match upload::drain(&config, leftovers, give_up_at) {
        None => store::remove(claimed),
        Some(mut rest) => {
            let _ = save(&mut claimed, &mut rest);
        }
    }
}

/// Close every file descriptor inherited beyond stdin, stdout and stderr
/// (which are the null device). One left open by whatever started the app,
/// like a pipe collecting its output, would otherwise stay open as long as
/// the uploader runs, making the app look like it never finished.
fn close_inherited_files() {
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Collected first, since listing the folder uses a descriptor too.
    let fds: Vec<i32> = entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    for fd in fds {
        // SAFETY: nothing in this process owns these yet: they were
        // inherited, and the uploader runs before anything else is opened.
        let _ = nix::unistd::close(fd);
    }
}
