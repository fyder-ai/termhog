//! A `std::process::Command` look-alike whose configuration we can read back.
//!
//! std keeps a Command's stdio private, so there is no way to learn where the
//! caller wanted each stream to go. Owning the builder lets every setting be
//! honored exactly as std would, while we interpose only on the streams the
//! caller left inherited.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout};

/// Where a child's stdin, stdout, or stderr goes. Mirrors [`std::process::Stdio`].
#[derive(Debug)]
pub struct HogStdio(pub(crate) StdioKind);

#[derive(Debug)]
pub(crate) enum StdioKind {
    Inherit,
    Null,
    Piped,
    /// A file, pipe end, or other open file the stream is connected to.
    Fd(OwnedFd),
}

impl HogStdio {
    /// The child uses the parent's stream. Only inherited stdout/stderr are recorded.
    pub fn inherit() -> HogStdio {
        HogStdio(StdioKind::Inherit)
    }

    /// The child's stream is connected to the null device.
    pub fn null() -> HogStdio {
        HogStdio(StdioKind::Null)
    }

    /// A pipe is opened to the child, exposed on [`crate::Recording`].
    pub fn piped() -> HogStdio {
        HogStdio(StdioKind::Piped)
    }

    pub(crate) fn is_inherit(&self) -> bool {
        matches!(self.0, StdioKind::Inherit)
    }

    /// Convert to the std equivalent for spawning. An open file is cloned so
    /// the command stays reusable.
    pub(crate) fn to_std(&self) -> io::Result<std::process::Stdio> {
        Ok(match &self.0 {
            StdioKind::Inherit => std::process::Stdio::inherit(),
            StdioKind::Null => std::process::Stdio::null(),
            StdioKind::Piped => std::process::Stdio::piped(),
            StdioKind::Fd(fd) => fd.try_clone()?.into(),
        })
    }
}

/// Connect a stream to an open file, pipe end, or the like, as std does.
macro_rules! stdio_from {
    ($($ty:ty),*) => {$(
        impl From<$ty> for HogStdio {
            fn from(value: $ty) -> HogStdio {
                HogStdio(StdioKind::Fd(value.into()))
            }
        }
    )*};
}

stdio_from!(File, OwnedFd, ChildStdin, ChildStdout, ChildStderr);

/// A process builder with the same API and defaults as [`std::process::Command`].
/// Every stream defaults to inherit.
#[derive(Debug)]
pub struct HogCommand {
    pub(crate) program: OsString,
    args: Vec<OsString>,
    /// Ordered env edits. `None` removes the variable.
    envs: Vec<(OsString, Option<OsString>)>,
    env_clear: bool,
    current_dir: Option<PathBuf>,
    pub(crate) stdin: HogStdio,
    pub(crate) stdout: HogStdio,
    pub(crate) stderr: HogStdio,
}

impl HogCommand {
    /// A command for running `program`, with no arguments, the parent's
    /// environment and working directory, and every stream inherited.
    pub fn new<S: AsRef<OsStr>>(program: S) -> HogCommand {
        HogCommand {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: Vec::new(),
            env_clear: false,
            current_dir: None,
            stdin: HogStdio::inherit(),
            stdout: HogStdio::inherit(),
            stderr: HogStdio::inherit(),
        }
    }

    /// Add an argument.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut HogCommand {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Add arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut HogCommand
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Set an environment variable for the child.
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut HogCommand
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.envs
            .push((key.as_ref().to_owned(), Some(val.as_ref().to_owned())));
        self
    }

    /// Set environment variables for the child.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut HogCommand
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.env(key, val);
        }
        self
    }

    /// Remove an environment variable from the child's environment.
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut HogCommand {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    /// Start the child with an empty environment, plus any variables set
    /// after this.
    pub fn env_clear(&mut self) -> &mut HogCommand {
        self.env_clear = true;
        self.envs.clear();
        self
    }

    /// Set the child's working directory.
    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut HogCommand {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// Where the child's stdin comes from. Defaults to inherit.
    pub fn stdin<T: Into<HogStdio>>(&mut self, cfg: T) -> &mut HogCommand {
        self.stdin = cfg.into();
        self
    }

    /// Where the child's stdout goes. Defaults to inherit, which is recorded.
    pub fn stdout<T: Into<HogStdio>>(&mut self, cfg: T) -> &mut HogCommand {
        self.stdout = cfg.into();
        self
    }

    /// Where the child's stderr goes. Defaults to inherit, which is recorded.
    pub fn stderr<T: Into<HogStdio>>(&mut self, cfg: T) -> &mut HogCommand {
        self.stderr = cfg.into();
        self
    }

    /// The program to run.
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    /// The arguments, not including the program.
    pub fn get_args(&self) -> impl ExactSizeIterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    /// The environment changes, in the order they apply. `None` removes the
    /// variable. Like std, this doesn't include [`HogCommand::env_clear`].
    pub fn get_envs(&self) -> impl ExactSizeIterator<Item = (&OsStr, Option<&OsStr>)> {
        self.envs
            .iter()
            .map(|(key, val)| (key.as_os_str(), val.as_deref()))
    }

    /// The working directory, if one was set.
    pub fn get_current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// A std command with everything but stdio applied. The session decides
    /// stdio per stream, since inherited streams may be interposed.
    pub(crate) fn to_std(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        if self.env_clear {
            cmd.env_clear();
        }
        for (key, val) in &self.envs {
            match val {
                Some(val) => cmd.env(key, val),
                None => cmd.env_remove(key),
            };
        }
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// The command line as one display string, for analytics metadata.
    pub(crate) fn display(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.get_args())
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
