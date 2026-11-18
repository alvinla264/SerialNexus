//! A small MCP-over-stdio client for the real `serial-nexus-mcp` binary.
//!
//! The binary is started exactly as an AI host starts it, with the client
//! terminal's gateway socket and a private pairing file in its environment.
//! Its stderr is discarded rather than logged: a provider's diagnostic is not
//! worth risking pairing material in a container log.
//!
//! This is a test harness client, not a second protocol implementation: it
//! only speaks JSON-RPC framing, `initialize`, `tools/list` and `tools/call`.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Why an MCP request did not produce a result.
///
/// The distinction matters at the shutdown assertion: a wedged server that
/// never answers is a timeout, not evidence that a remote connection closed.
pub enum McpError {
    Timeout(String),
    /// The server's stdout closed, so the process (and its connections) ended.
    Closed(String),
    /// The server answered with a JSON-RPC error or a malformed answer.
    Protocol(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(message) => write!(formatter, "timed out: {message}"),
            Self::Closed(message) => write!(formatter, "connection closed: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
        }
    }
}

impl From<McpError> for String {
    fn from(error: McpError) -> Self {
        error.to_string()
    }
}

/// One tool call's answer.
pub struct Outcome {
    pub is_error: bool,
    pub text: String,
    pub structured: Value,
}

pub struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    /// Close after this many seconds even if the child is wedged.
    pub timeout: Duration,
}

impl Mcp {
    pub fn spawn(
        binary: &Path,
        environment: &[(String, String)],
        removed: &[&str],
    ) -> Result<Self, String> {
        let mut command = Command::new(binary);
        for name in removed {
            command.env_remove(name);
        }
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
        let stdin = child.stdin.take().ok_or("mcp stdin is missing")?;
        let stdout: ChildStdout = child.stdout.take().ok_or("mcp stdout is missing")?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if sender.send(line.clone()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 1,
            timeout: Duration::from_secs(30),
        })
    }

    /// Send a request and wait for the matching answer, ignoring notifications.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Closed("mcp stdin is closed".to_string()))?;
        serde_json::to_writer(&mut *stdin, &message)
            .map_err(|error| McpError::Protocol(format!("{method}: {error}")))?;
        stdin
            .write_all(b"\n")
            .map_err(|error| McpError::Closed(format!("{method}: {error}")))?;
        stdin
            .flush()
            .map_err(|error| McpError::Closed(format!("{method}: {error}")))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(McpError::Timeout(format!(
                    "{method}: no answer within {:?}",
                    self.timeout
                )));
            }
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(McpError::Timeout(format!(
                        "{method}: no answer within {:?}",
                        self.timeout
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Closed(format!(
                        "{method}: the MCP server closed its stdout"
                    )))
                }
            };
            let value: Value = match serde_json::from_str(line.trim()) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(McpError::Protocol(format!(
                    "{method}: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("protocol error")
                )));
            }
            return value.get("result").cloned().ok_or_else(|| {
                McpError::Protocol(format!("{method}: the answer has neither result nor error"))
            });
        }
    }

    pub fn initialize(&mut self) -> Result<Value, McpError> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "serial-nexus-lan-gate", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        self.notify(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(result)
    }

    fn notify(&mut self, message: Value) -> Result<(), McpError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Closed("mcp stdin is closed".to_string()))?;
        serde_json::to_writer(&mut *stdin, &message)
            .map_err(|error| McpError::Protocol(error.to_string()))?;
        stdin
            .write_all(b"\n")
            .map_err(|error| McpError::Closed(error.to_string()))?;
        stdin
            .flush()
            .map_err(|error| McpError::Closed(error.to_string()))
    }

    /// A request that needs no device; proves the server is still responsive.
    pub fn ping(&mut self) -> Result<(), McpError> {
        self.request("ping", json!({})).map(|_| ())
    }

    pub fn list_tools(&mut self) -> Result<Value, McpError> {
        self.request("tools/list", json!({}))
    }

    pub fn call(&mut self, name: &str, arguments: Value) -> Result<Outcome, McpError> {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let structured = result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(Outcome {
            is_error,
            text,
            structured,
        })
    }

    /// Close stdin and wait briefly; the child is killed if it does not leave.
    pub fn stop(mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
