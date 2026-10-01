//! Framework-owned terminal sessions shared by Agent tools and host terminal adapters.
use super::pty_process::{PtyCommand, PtyProcess};
use eliteforge_agent_core::ToolError;
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf};
use std::{
    collections::hash_map::DefaultHasher,
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    io::{Read, Write},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
pub struct PtyStart {
    pub owner: String,
    pub invocation_id: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub program: String,
    pub program_args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySnapshot {
    pub terminal_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub running: bool,
    pub exit_code: Option<u32>,
    pub output: String,
    pub next_cursor: u64,
    pub truncated: bool,
    pub output_closed: bool,
}
/// Raw bytes preserve escape sequences and split UTF-8 characters for host transports.
pub enum PtyEvent {
    Output(Vec<u8>),
    Exit(Option<u32>),
}

const BUFFER_LIMIT: usize = 256 * 1024;
const PAGE_LIMIT: usize = 32 * 1024;
fn fail(error: impl ToString) -> ToolError {
    ToolError::Failed(error.to_string())
}
#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    offset: u64,
    closed: bool,
}
struct Session {
    command: String,
    args: Vec<String>,
    owner: String,
    cwd: String,
    process: Mutex<PtyProcess>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    output: Arc<Mutex<Output>>,
}
impl Session {
    fn stop(&self) -> Result<(), ToolError> {
        let mut process = self.process.lock().map_err(fail)?;
        process.kill().map_err(fail)?;
        process.wait().map_err(fail)?;
        self.writer.lock().map_err(fail)?.take();
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
#[derive(Clone, Default)]
pub struct PtyService {
    registry: Arc<Mutex<SessionRegistry>>,
}
#[derive(Default)]
struct SessionRegistry {
    closed: bool,
    sessions: HashMap<String, Arc<Session>>,
    invocations: HashMap<(String, String), InvocationRecord>,
}
struct InvocationRecord {
    terminal_id: String,
    fingerprint: u64,
}
impl PtyService {
    fn session(&self, owner: &str, id: &str) -> Result<Arc<Session>, ToolError> {
        self.registry
            .lock()
            .map_err(fail)?
            .sessions
            .get(id)
            .filter(|session| session.owner == owner)
            .cloned()
            .ok_or_else(|| fail("terminal session not found for this owner"))
    }
    fn snapshot(id: &str, session: &Session, cursor: u64) -> Result<PtySnapshot, ToolError> {
        let status = session
            .process
            .lock()
            .map_err(fail)?
            .try_wait()
            .map_err(fail)?;
        let output = session.output.lock().map_err(fail)?;
        let end = output.offset + output.bytes.len() as u64;
        if cursor > end {
            return Err(fail("output cursor is beyond available output"));
        }
        let start = cursor.max(output.offset);
        let mut bytes: Vec<_> = output
            .bytes
            .iter()
            .skip((start - output.offset) as usize)
            .take(PAGE_LIMIT)
            .copied()
            .collect();
        // Do not consume an incomplete UTF-8 character at a page/read boundary.
        // Invalid bytes still use lossy decoding; a closed stream cannot complete a suffix.
        if let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_none()
            && (!output.closed || start + (bytes.len() as u64) < end)
        {
            bytes.truncate(error.valid_up_to());
        }
        Ok(PtySnapshot {
            terminal_id: id.into(),
            command: session.command.clone(),
            args: session.args.clone(),
            cwd: session.cwd.clone(),
            running: status.is_none(),
            exit_code: status.map(|status| status.exit_code()),
            output: String::from_utf8_lossy(&bytes).into_owned(),
            next_cursor: start + bytes.len() as u64,
            truncated: cursor < output.offset,
            output_closed: output.closed,
        })
    }
}
impl PtyService {
    pub fn start(&self, request: PtyStart) -> Result<PtySnapshot, ToolError> {
        let mut command = PtyCommand::new(&request.program);
        command.args(&request.program_args);
        command.cwd(&request.cwd);
        command.env("TERM", "xterm-256color");
        for (key, value) in &request.env {
            command.env(key, value);
        }
        self.spawn(request, command, 100, 24, None)
    }
    /// The caller supplies an already-authorized owner and command. The framework
    /// owns process lifetime; dropping an output receiver does not stop a session.
    pub fn start_stream(
        &self,
        owner: String,
        command: PtyCommand,
        cols: u16,
        rows: u16,
    ) -> Result<(PtySnapshot, UnboundedReceiver<PtyEvent>), ToolError> {
        let cwd = command
            .cwd
            .clone()
            .ok_or_else(|| fail("PTY working directory is required"))?;
        let request = PtyStart {
            owner,
            invocation_id: None,
            command: command.program.to_string_lossy().into_owned(),
            args: command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            program: command.program.to_string_lossy().into_owned(),
            program_args: Vec::new(),
            cwd,
            env: BTreeMap::new(),
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        Ok((
            self.spawn(request, command, cols, rows, Some(sender))?,
            receiver,
        ))
    }
    fn spawn(
        &self,
        request: PtyStart,
        command: PtyCommand,
        cols: u16,
        rows: u16,
        events: Option<UnboundedSender<PtyEvent>>,
    ) -> Result<PtySnapshot, ToolError> {
        let mut registry = self.registry.lock().map_err(fail)?;
        if registry.closed {
            return Err(fail("PTY service is closed"));
        }
        let invocation_key = request
            .invocation_id
            .as_ref()
            .map(|id| (request.owner.clone(), id.clone()));
        let fingerprint = terminal_start_fingerprint(&request);
        if let Some(key) = invocation_key.as_ref()
            && let Some(record) = registry.invocations.get(key)
        {
            if record.fingerprint != fingerprint {
                return Err(fail(
                    "terminal_start invocation was retried with different input",
                ));
            }
            let session = registry.sessions.get(&record.terminal_id).ok_or_else(|| {
                fail("terminal session for this invocation has expired; it was not restarted")
            })?;
            return Self::snapshot(&record.terminal_id, session, 0);
        }
        // Completed sessions are reclaimed only when capacity is needed.
        if registry.sessions.len() >= 64
            || registry
                .sessions
                .values()
                .filter(|session| session.owner == request.owner)
                .count()
                >= 8
        {
            registry.sessions.retain(|_, session| {
                session
                    .process
                    .lock()
                    .ok()
                    .and_then(|mut child| child.try_wait().ok())
                    .flatten()
                    .is_none()
            });
        }
        if registry.sessions.len() >= 64
            || registry
                .sessions
                .values()
                .filter(|session| session.owner == request.owner)
                .count()
                >= 8
        {
            return Err(fail(
                "terminal capacity reached; stop existing sessions before starting more",
            ));
        }
        if !request.cwd.is_dir() {
            return Err(fail("PTY working directory does not exist"));
        }
        let (process, reader, writer) = PtyProcess::spawn(&command, cols, rows).map_err(fail)?;
        let output = Arc::new(Mutex::new(Output::default()));
        let session = Arc::new(Session {
            command: request.command,
            args: request.args,
            owner: request.owner,
            cwd: request.cwd.display().to_string(),
            process: Mutex::new(process),
            writer: Mutex::new(Some(writer)),
            output: output.clone(),
        });
        let output_events = events.clone();
        std::thread::Builder::new()
            .name("framework-pty-output".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buffer = [0; 8192];
                while let Ok(size) = reader.read(&mut buffer) {
                    if size == 0 {
                        break;
                    }
                    let Ok(mut output) = output.lock() else {
                        return;
                    };
                    output.bytes.extend(&buffer[..size]);
                    let excess = output.bytes.len().saturating_sub(BUFFER_LIMIT);
                    output.bytes.drain(..excess);
                    output.offset += excess as u64;
                    drop(output);
                    if let Some(events) = &output_events {
                        let _ = events.send(PtyEvent::Output(buffer[..size].to_vec()));
                    }
                }
                if let Ok(mut output) = output.lock() {
                    output.closed = true;
                }
            })
            .map_err(fail)?;
        // A separate monitor closes ConPTY on process exit; waiting for output
        // EOF first can deadlock on Windows. Weak ownership lets close/drop kill
        // the child even when the output consumer has disappeared.
        let weak_session = Arc::downgrade(&session);
        std::thread::Builder::new()
            .name("framework-pty-exit".into())
            .spawn(move || {
                while let Some(session) = weak_session.upgrade() {
                    let status = session.process.lock().ok().and_then(|mut process| {
                        let status = process.try_wait().ok().flatten();
                        if status.is_some() {
                            process.close_terminal();
                        }
                        status
                    });
                    if status.is_some()
                        && let Ok(mut writer) = session.writer.lock()
                    {
                        writer.take();
                    }
                    let closed = session
                        .output
                        .lock()
                        .map(|output| output.closed)
                        .unwrap_or(true);
                    drop(session);
                    if let Some(status) = status.filter(|_| closed) {
                        if let Some(events) = &events {
                            let _ = events.send(PtyEvent::Exit(Some(status.exit_code())));
                        }
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            })
            .map_err(fail)?;
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = format!(
            "terminal-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let snapshot = Self::snapshot(&id, &session, 0)?;
        registry.sessions.insert(id.clone(), session);
        if let Some(key) = invocation_key {
            registry.invocations.insert(
                key,
                InvocationRecord {
                    terminal_id: id,
                    fingerprint,
                },
            );
        }
        Ok(snapshot)
    }
    pub fn read(&self, owner: &str, id: &str, cursor: u64) -> Result<PtySnapshot, ToolError> {
        Self::snapshot(id, self.session(owner, id)?.as_ref(), cursor)
    }
    pub fn write(&self, owner: &str, id: &str, data: &str) -> Result<(), ToolError> {
        if data.len() > 65536 {
            return Err(fail("terminal input exceeds 64 KiB"));
        }
        let session = self.session(owner, id)?;
        if session
            .process
            .lock()
            .map_err(fail)?
            .try_wait()
            .map_err(fail)?
            .is_some()
        {
            return Err(fail("terminal process has exited"));
        }
        let mut writer = session.writer.lock().map_err(fail)?;
        let writer = writer
            .as_mut()
            .ok_or_else(|| fail("terminal input is closed"))?;
        writer.write_all(data.as_bytes()).map_err(fail)?;
        writer.flush().map_err(fail)
    }
    pub fn stop(&self, owner: &str, id: &str) -> Result<(), ToolError> {
        self.session(owner, id)?.stop()
    }

    pub fn resize(&self, owner: &str, id: &str, cols: u16, rows: u16) -> Result<(), ToolError> {
        self.session(owner, id)?
            .process
            .lock()
            .map_err(fail)?
            .resize(cols, rows)
            .map_err(fail)
    }
    /// Release a host terminal and its retained output. Invocation tombstones
    /// remain so an old model call cannot accidentally start a second process.
    pub fn release(&self, owner: &str, id: &str) -> Result<(), ToolError> {
        let mut registry = self.registry.lock().map_err(fail)?;
        let session = registry
            .sessions
            .get(id)
            .filter(|session| session.owner == owner)
            .ok_or_else(|| fail("terminal session not found for this owner"))?;
        session.stop()?;
        registry.sessions.remove(id);
        Ok(())
    }
    pub fn close(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.closed = true;
            for session in registry.sessions.values() {
                let _ = session.stop();
            }
            registry.sessions.clear();
        }
    }
    pub fn list(&self, owner: &str) -> Result<Vec<PtySnapshot>, ToolError> {
        let registry = self.registry.lock().map_err(fail)?;
        registry
            .sessions
            .iter()
            .filter(|(_, session)| session.owner == owner)
            .map(|(id, session)| {
                let end = {
                    let output = session.output.lock().map_err(fail)?;
                    output.offset + output.bytes.len() as u64
                };
                let mut snapshot = Self::snapshot(id, session, end)?;
                snapshot.output.clear();
                Ok(snapshot)
            })
            .collect()
    }
}

fn terminal_start_fingerprint(request: &PtyStart) -> u64 {
    let mut hasher = DefaultHasher::new();
    request.owner.hash(&mut hasher);
    request.command.hash(&mut hasher);
    request.args.hash(&mut hasher);
    request.program.hash(&mut hasher);
    request.program_args.hash(&mut hasher);
    request.cwd.hash(&mut hasher);
    request.env.hash(&mut hasher);
    hasher.finish()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, path::PathBuf};

    fn request(invocation_id: &str, command: &str) -> PtyStart {
        PtyStart {
            owner: "owner-a".into(),
            invocation_id: Some(invocation_id.into()),
            command: command.into(),
            args: Vec::new(),
            program: "/bin/sh".into(),
            program_args: vec!["-c".into(), "sleep 30".into()],
            cwd: PathBuf::from("/tmp"),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn starting_the_same_call_is_idempotent_and_eviction_never_restarts_it() {
        let terminal = PtyService::default();
        let first = terminal
            .start(request("run-a:call-a", "service"))
            .expect("first start should succeed");
        let retry = terminal
            .start(request("run-a:call-a", "service"))
            .expect("retry should return the existing terminal");
        assert_eq!(first.terminal_id, retry.terminal_id);

        let separate = terminal
            .start(request("run-a:call-b", "service"))
            .expect("a distinct tool call may start the same command");
        assert_ne!(first.terminal_id, separate.terminal_id);

        terminal
            .registry
            .lock()
            .unwrap()
            .sessions
            .remove(&first.terminal_id);
        let expired = terminal
            .start(request("run-a:call-a", "service"))
            .err()
            .expect("an evicted invocation must not launch a replacement");
        assert!(expired.to_string().contains("was not restarted"));
        terminal.stop("owner-a", &separate.terminal_id).unwrap();
    }

    #[test]
    fn a_retry_with_different_command_is_rejected() {
        let terminal = PtyService::default();
        let first = terminal.start(request("run-a:call-a", "service"));
        assert!(first.is_ok());
        let error = terminal
            .start(request("run-a:call-a", "different"))
            .err()
            .expect("call identity cannot be reused for different input");
        assert!(error.to_string().contains("different input"));
    }
}

/// Registers a host-shared terminal service with Agent lifecycle cleanup.
pub struct PtyPlugin(pub PtyService);
#[async_trait::async_trait]
impl eliteforge_agent_core::Plugin for PtyPlugin {
    fn id(&self) -> &str {
        "framework.pty.service"
    }
    async fn install(
        &self,
        context: &mut eliteforge_agent_core::PluginContext<'_>,
    ) -> Result<(), eliteforge_agent_core::PluginError> {
        context.provide(self.0.clone())?;
        let cleanup = self.0.clone();
        context.on_cleanup(move || cleanup.close());
        let shutdown = self.0.clone();
        context.on_shutdown(move || async move {
            tokio::task::spawn_blocking(move || shutdown.close())
                .await
                .map_err(|error| error.to_string())
        });
        Ok(())
    }
}
