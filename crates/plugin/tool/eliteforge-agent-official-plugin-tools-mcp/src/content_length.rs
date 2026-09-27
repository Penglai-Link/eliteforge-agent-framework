//! Legacy Content-Length framing, using the same MCP SDK session as JSON lines.
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use std::{io, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};

pub(crate) struct ContentLengthTransport {
    child: Option<Child>,
    reader: BufReader<ChildStdout>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    limit: usize,
}
impl ContentLengthTransport {
    pub fn spawn(mut command: tokio::process::Command, limit: usize) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let reader = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing stdout"))?,
        );
        let writer = Arc::new(Mutex::new(child.stdin.take()));
        Ok(Self {
            child: Some(child),
            reader,
            writer,
            limit,
        })
    }
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }
    async fn read_frame(&mut self) -> io::Result<RxJsonRpcMessage<RoleClient>> {
        let mut length = None;
        let mut header_bytes = 0;
        loop {
            let mut line = Vec::new();
            // Bound header allocation even when a peer never sends a newline.
            let read = (&mut self.reader)
                .take(8193 - header_bytes)
                .read_until(b'\n', &mut line)
                .await?;
            header_bytes += read as u64;
            if read == 0 || header_bytes > 8192 {
                return Err(io::Error::other("invalid MCP frame header"));
            }
            let line = std::str::from_utf8(&line).map_err(io::Error::other)?.trim();
            if line.is_empty() {
                break;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| io::Error::other("invalid MCP frame header"))?;
            if key.eq_ignore_ascii_case("content-length") {
                if length.is_some() {
                    return Err(io::Error::other("duplicate MCP frame length"));
                }
                length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
            }
        }
        let length = length
            .filter(|n| *n > 0 && *n <= self.limit)
            .ok_or_else(|| io::Error::other("invalid MCP frame length"))?;
        let mut body = vec![0; length];
        self.reader.read_exact(&mut body).await?;
        serde_json::from_slice(&body).map_err(io::Error::other)
    }
}
impl Transport<RoleClient> for ContentLengthTransport {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let writer = self.writer.clone();
        let limit = self.limit;
        async move {
            let body = serde_json::to_vec(&item).map_err(io::Error::other)?;
            if body.len() > limit {
                return Err(io::Error::other("MCP frame too large"));
            }
            let mut guard = writer.lock().await;
            let writer = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("MCP transport closed"))?;
            writer
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .await?;
            writer.write_all(&body).await?;
            writer.flush().await
        }
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.read_frame().await.ok()
    }
    async fn close(&mut self) -> io::Result<()> {
        self.writer.lock().await.take();
        if let Some(mut child) = self.child.take() {
            child.kill().await?;
        }
        Ok(())
    }
}
impl Drop for ContentLengthTransport {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.kill().await;
                });
            }
        }
    }
}
