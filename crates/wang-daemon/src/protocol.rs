//! Wire protocol between `wang chat` and the daemon: newline-delimited JSON over a Unix socket.

use serde::{Deserialize, Serialize};

/// Client → daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Run one task in the named session (conversation).
    Message { session: String, text: String },
    /// Drop the session's history.
    New { session: String },
    /// Answer a pending approval request.
    Approve { id: String, allow: bool },
}

/// Daemon → client. A `Message` produces a stream of events ending in `Done` or `Error`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// A piece of the model's reply, streamed as it is generated.
    Text { delta: String },
    ToolStart { name: String, detail: String },
    ToolEnd { name: String, ok: bool, ms: u64, trimmed: bool, repeated: bool },
    /// A privileged action is waiting for your yes/no (phone or terminal).
    ApprovalRequest { id: String, tool: String, detail: String },
    /// approved | denied | timed_out
    ApprovalResult { id: String, decision: String },
    Done { task_id: i64, footer: String },
    Error { message: String },
    Ok,
}
