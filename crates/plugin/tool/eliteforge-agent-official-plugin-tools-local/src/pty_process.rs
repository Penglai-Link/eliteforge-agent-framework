//! The single native PTY boundary shared by command tools and terminal sessions.
use crate::process_tree::ProcessTree;
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{Read, Write},
    path::PathBuf,
};

/// An inert command description. Constructing it never starts a process.
#[derive(Clone, Debug)]
pub struct PtyCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
}
impl PtyCommand {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) {
        self.args.push(arg.as_ref().into());
    }
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().into()));
    }
    pub fn cwd(&mut self, cwd: impl Into<PathBuf>) {
        self.cwd = Some(cwd.into());
    }
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        self.env.insert(key.as_ref().into(), value.as_ref().into());
    }
}

pub(crate) struct PtyProcess {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    tree: Option<ProcessTree>,
    finished: bool,
}
type Io = (PtyProcess, Box<dyn Read + Send>, Box<dyn Write + Send>);
impl PtyProcess {
    pub(crate) fn spawn(spec: &PtyCommand, cols: u16, rows: u16) -> std::io::Result<Io> {
        fn fail(error: impl ToString) -> std::io::Error {
            std::io::Error::other(error.to_string())
        }
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(fail)?;
        // Acquire handles before starting a child, so acquisition errors cannot orphan it.
        let reader = pair.master.try_clone_reader().map_err(fail)?;
        let writer = pair.master.take_writer().map_err(fail)?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.cwd(cwd);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).map_err(fail)?;
        let mut process = Self {
            child,
            master: Some(pair.master),
            tree: None,
            finished: false,
        };
        let pid = process
            .process_id()
            .ok_or_else(|| std::io::Error::other("PTY has no process ID"))?;
        process.tree = Some(ProcessTree::attach(pid)?);
        Ok((process, reader, writer))
    }
    pub(crate) fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.finished |= status.is_some();
        if self.finished {
            self.terminate_tree();
        }
        Ok(status)
    }
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.master
            .as_ref()
            .ok_or_else(|| std::io::Error::other("PTY is closed"))?
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
    fn terminate_tree(&mut self) {
        let Some(mut tree) = self.tree.take() else {
            return;
        };
        #[cfg(unix)]
        if let Some(group) = self
            .master
            .as_ref()
            .and_then(|master| master.process_group_leader())
            .filter(|group| *group > 1)
        {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        tree.terminate();
    }
    pub(crate) fn close_terminal(&mut self) {
        self.master.take();
    }
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        let status = self.try_wait()?;
        self.terminate_tree();
        if status.is_none() && self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        self.close_terminal();
        Ok(())
    }
    pub(crate) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.finished = true;
        Ok(status)
    }
}
impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.kill();
        if !self.finished {
            let _ = self.wait();
        }
    }
}
