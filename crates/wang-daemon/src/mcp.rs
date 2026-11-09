//! `wang mcp`: wang's tools as an MCP server over stdio, for Claude Code and Claude Desktop.
//!
//! Claude does the thinking (on the user's own subscription); wang supplies the hands: a
//! sandboxed shell and file reader, web_fetch behind an allow-list, the jailed browser,
//! throwaway micro-VMs, memory, and the cost ledger. The task workflows are offered as MCP
//! prompts, which Claude Code shows as slash commands (`/mcp__wang__research`, ...).
//!
//! Protocol: JSON-RPC 2.0, one message per line on stdin/stdout. Logs go to stderr only.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use wang_tokens::{Ledger, cap_middle};

use crate::tools::{self, ToolCtx};
use crate::workflow::Workflow;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct McpServer {
    pub ctx: ToolCtx,
    pub ledger: Option<Arc<Ledger>>,
    /// Browser and VM state persist per server process (one Claude session).
    pub session: String,
    /// Let privileged shell commands (writes, network) through; Claude's own approval prompt
    /// is then the only check. Off by default.
    pub allow_privileged: bool,
    pub output_cap: usize,
}

const INSTRUCTIONS: &str = "wang runs tools in a sandbox on this Linux machine. shell and read_file see the user's files \
read-only with secrets hidden and no network (unless privileged commands are enabled); web_fetch and browser can reach only \
allowed domains; vm runs commands in a throwaway micro-VM (root, no access to the user's files) with checkpoint, rollback \
and try (run alternatives in parallel copies, keep the best). Prefer vm for installing packages, running untrusted or \
generated code, and experiments. The browser shows pages as compact text where links, buttons and fields are numbered [n].";

impl McpServer {
    fn tool_list(&self) -> Vec<Value> {
        let mut out: Vec<Value> = tools::definitions_for(&self.ctx)
            .into_iter()
            .map(|d| {
                let mut schema = d["input_schema"].clone();
                let mut desc = d["description"].as_str().unwrap_or("").to_string();
                if d["name"] == "shell" && !self.allow_privileged {
                    if let Some(p) = schema["properties"].as_object_mut() {
                        p.remove("privileged");
                    }
                    desc += " In this MCP server, commands are read-only with no network; use the vm tool for anything that installs, writes or downloads.";
                }
                let read_only = matches!(d["name"].as_str(), Some("read_file" | "recall" | "web_fetch"));
                json!({"name": d["name"], "description": desc, "inputSchema": schema,
                       "annotations": {"readOnlyHint": read_only, "openWorldHint": matches!(d["name"].as_str(), Some("web_fetch" | "browser" | "vm"))}})
            })
            .collect();
        if self.ledger.is_some() {
            out.push(json!({
                "name": "cost",
                "description": "wang's cost ledger: tokens, cache hits and $ spent (wang's own agent, and Claude Code / opencode traffic sent through `wang proxy`).",
                "inputSchema": {"type": "object", "properties": {"period": {"type": "string", "enum": ["today", "week", "month"]}}},
                "annotations": {"readOnlyHint": true},
            }));
        }
        out
    }

    async fn call_tool(&self, name: &str, args: &Value) -> (String, bool) {
        if name == "cost" {
            let Some(l) = &self.ledger else { return ("no ledger".into(), true) };
            let (days, label) = match args["period"].as_str() {
                Some("week") => (7, "last 7 days"),
                Some("month") => (30, "last 30 days"),
                _ => (1, "today"),
            };
            let mut text = match l.summary(days, label) {
                Ok(s) => s.to_string(),
                Err(e) => return (format!("{e:#}"), true),
            };
            if let Ok(p) = l.proxy_summary(days, label)
                && !p.clients.is_empty()
            {
                text += &format!("\n{p}");
            }
            return (text, false);
        }
        if !tools::definitions_for(&self.ctx).iter().any(|d| d["name"] == name) {
            return (format!("unknown tool: {name}"), true);
        }
        if self.ctx.needs_approval(name, args) && !self.allow_privileged {
            return (
                "This command needs write or network access, which wang's MCP server doesn't allow. Run it in the vm tool instead, \
                 or ask the user to set [mcp] allow_privileged = true in ~/.config/wang/config.toml."
                    .into(),
                true,
            );
        }
        let out = tools::run_in(name, args, &self.ctx, &self.session).await;
        (cap_middle(&out.text, self.output_cap).text, out.is_error)
    }

    fn prompts(&self) -> Vec<Value> {
        [Workflow::Research, Workflow::Code, Workflow::Quick]
            .into_iter()
            .map(|w| {
                json!({"name": w.name(), "description": format!("wang workflow: {}", w.summary()),
                       "arguments": [{"name": "task", "description": "What you want done", "required": true}]})
            })
            .collect()
    }

    fn prompt(&self, name: &str, args: &Value) -> Option<Value> {
        let w = Workflow::from_name(name).filter(|w| *w != Workflow::General)?;
        let task = args["task"].as_str().unwrap_or("").trim();
        let tools = w.allowed(self.ctx.vm.is_some()).map(|t| t.join(", ")).unwrap_or_else(|| "any".into());
        let text = format!("{task}\n\n[wang workflow: {}] {}\nUse wang's MCP tools for this ({tools}).", w.name(), w.guidance(self.ctx.vm.is_some()));
        Some(json!({"description": w.summary(), "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
    }

    /// Handle one JSON-RPC message; `None` for notifications (no reply).
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned()?;
        let method = msg["method"].as_str().unwrap_or("");
        let params = &msg["params"];
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}, "prompts": {"listChanged": false}},
                    "serverInfo": {"name": "wang", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tool_list()})),
            "tools/call" => match params["name"].as_str() {
                Some(name) => {
                    let args = if params["arguments"].is_object() { params["arguments"].clone() } else { json!({}) };
                    let (text, is_error) = self.call_tool(name, &args).await;
                    tracing::info!(tool = name, error = is_error, bytes = text.len(), "mcp tool call");
                    Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
                }
                None => Err((-32602, "tools/call needs 'name'".into())),
            },
            "prompts/list" => Ok(json!({"prompts": self.prompts()})),
            "prompts/get" => match self.prompt(params["name"].as_str().unwrap_or(""), &params["arguments"]) {
                Some(p) => Ok(p),
                None => Err((-32602, format!("unknown prompt '{}'", params["name"].as_str().unwrap_or("")))),
            },
            "resources/list" => Ok(json!({"resources": []})),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    /// Serve until stdin closes. Requests run concurrently (Claude may call tools in parallel).
    pub async fn serve_stdio(self: Arc<Self>) -> Result<()> {
        let out = Arc::new(Mutex::new(tokio::io::stdout()));
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut tasks = tokio::task::JoinSet::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                    write(&out, &err).await;
                    continue;
                }
            };
            let (me, out) = (self.clone(), out.clone());
            tasks.spawn(async move {
                if let Some(reply) = me.handle(&msg).await {
                    write(&out, &reply).await;
                }
            });
            while tasks.try_join_next().is_some() {}
        }
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    /// Close the browsers and VMs this session started.
    pub async fn cleanup(&self) {
        if let Some(p) = &self.ctx.browser {
            p.shutdown().await;
        }
        if let Some(v) = &self.ctx.vm {
            v.shutdown().await;
        }
    }
}

async fn write(out: &Mutex<tokio::io::Stdout>, v: &Value) {
    let mut o = out.lock().await;
    let _ = o.write_all(format!("{v}\n").as_bytes()).await;
    let _ = o.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Sandbox, WebPolicy};

    fn server() -> McpServer {
        let ctx = ToolCtx::with(std::env::temp_dir(), Sandbox::Off, WebPolicy::default());
        McpServer { ctx, ledger: Some(Arc::new(Ledger::in_memory().unwrap())), session: "mcp-test".into(), allow_privileged: false, output_cap: 40_000 }
    }

    async fn rpc(s: &McpServer, method: &str, params: Value) -> Value {
        s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).await.unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handshake_tools_prompts() {
        let s = server();
        let r = rpc(&s, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code"}})).await;
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "wang");
        assert!(r["result"]["capabilities"]["tools"].is_object() && r["result"]["capabilities"]["prompts"].is_object());
        assert_eq!(rpc(&s, "initialize", json!({"protocolVersion": "1999-01-01"})).await["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());

        let tools = rpc(&s, "tools/list", json!({})).await["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for n in ["read_file", "shell", "web_fetch", "remember", "recall", "cost"] {
            assert!(names.contains(&n), "{names:?}");
        }
        let shell = tools.iter().find(|t| t["name"] == "shell").unwrap();
        assert!(shell["inputSchema"]["properties"].get("privileged").is_none(), "privileged hidden by default");

        let p = rpc(&s, "prompts/list", json!({})).await;
        assert_eq!(p["result"]["prompts"].as_array().unwrap().len(), 3);
        let g = rpc(&s, "prompts/get", json!({"name": "research", "arguments": {"task": "best rust web framework"}})).await;
        let text = g["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.starts_with("best rust web framework") && text.contains("[wang workflow: research]"), "{text}");
        assert_eq!(rpc(&s, "nope", json!({})).await["error"]["code"], -32601);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_calls_and_privilege_gate() {
        let s = server();
        let r = rpc(&s, "tools/call", json!({"name": "cost", "arguments": {}})).await;
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("wang cost"), "{r}");
        // Sandbox off: every shell command would need approval, which MCP mode refuses.
        let r = rpc(&s, "tools/call", json!({"name": "shell", "arguments": {"command": "echo hi"}})).await;
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("vm tool"));
        let r = rpc(&s, "tools/call", json!({"name": "browser", "arguments": {"action": "view"}})).await;
        assert_eq!(r["result"]["isError"], true, "browser isn't configured here: {r}");
        let r = rpc(&s, "tools/call", json!({"name": "recall", "arguments": {"query": "x"}})).await;
        assert!(r["result"]["content"].is_array());
    }
}
