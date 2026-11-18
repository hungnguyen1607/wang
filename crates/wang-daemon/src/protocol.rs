//! Wire protocol between `wang chat` and the daemon: newline-delimited JSON over a Unix socket.

use serde::{Deserialize, Serialize};

/// Client → daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Run one task in the named session (conversation).
    Message {
        session: String,
        text: String,
        /// The reply will be read aloud (`wang listen`): ask for short spoken sentences.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        voice: bool,
    },
    /// Drop the session's history.
    New { session: String },
    /// Answer a pending approval request.
    Approve { id: String, allow: bool },
    /// From a brain's MCP proxy (`wang mcp --via-daemon`): wang's tool definitions.
    ToolList { session: String },
    /// From a brain's MCP proxy: run one tool in `session` (approvals, sandbox, ledger apply).
    ToolCall { session: String, name: String, input: serde_json::Value },
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
    /// Reply to `ToolList`: tool definitions (name, description, input_schema).
    Tools { tools: Vec<serde_json::Value> },
    /// Reply to `ToolCall`.
    ToolResult {
        text: String,
        is_error: bool,
        /// base64 JPEG (screenshots).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
    },
    Error { message: String },
    Ok,
}
