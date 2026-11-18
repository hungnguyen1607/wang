use super::*;
use crate::agent::tests::agent;
use crate::approvals::Approvals;
use crate::model::testing::Mock;
use crate::server::{Channel, Hub, UnixSocketChannel};

/// A real `claude -p --output-format stream-json --verbose --include-partial-messages` run
/// (Claude Code 2.1, subscription login), recorded and trimmed.
const CLAUDE_STREAM: &str = include_str!("claude-stream.jsonl");

#[test]
fn parses_a_real_claude_stream() {
    let evs: Vec<Ev> = CLAUDE_STREAM.lines().flat_map(parse_claude).collect();
    assert_eq!(evs[0], Ev::Session("4087aee8-dda3-4110-80ec-97579859bdac".into()));
    assert!(evs.contains(&Ev::NewMessage));
    assert!(evs.contains(&Ev::Text("pong".into())));
    let limit = evs.iter().find_map(|e| match e {
        Ev::Limit { status, kind, utilization, .. } => Some((status.clone(), kind.clone(), *utilization)),
        _ => None,
    });
    assert_eq!(limit, Some(("allowed".into(), "five_hour".into(), Some(0.86))));
    match evs.last().unwrap() {
        Ev::Done { result, api_equiv_usd, usage } => {
            assert_eq!(result.as_deref(), Some("pong"));
            assert!(*api_equiv_usd > 0.0);
            assert!(usage.output_tokens > 0);
        }
        e => panic!("{e:?}"),
    }
    // Errors and sub-agent text.
    assert_eq!(
        parse_claude(r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":""}"#),
        vec![Ev::Error("Claude Code stopped: error_max_turns".into())]
    );
    assert!(parse_claude(r#"{"type":"stream_event","parent_tool_use_id":"x","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub"}}}"#).is_empty());
    assert!(parse_claude("not json").is_empty());
}

#[test]
fn parses_codex_json_events() {
    assert_eq!(parse_codex(r#"{"type":"thread.started","thread_id":"t-1"}"#), vec![Ev::Session("t-1".into())]);
    assert_eq!(
        parse_codex(r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi there"}}"#),
        vec![Ev::NewMessage, Ev::Text("hi there".into())]
    );
    assert!(parse_codex(r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#).is_empty());
    match &parse_codex(r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":7}}"#)[0] {
        Ev::Done { usage, .. } => assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (40, 60, 7)),
        e => panic!("{e:?}"),
    }
    assert_eq!(parse_codex(r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#), vec![Ev::Error("usage limit".into())]);
}

fn brain(kind: Kind, bin: &str) -> CliBrain {
    CliBrain {
        kind,
        bin: bin.into(),
        model: None,
        wang_exe: "/opt/wang".into(),
        socket: "/run/user/1/wang.sock".into(),
        jail: None,
        timeout: Duration::from_secs(20),
    }
}

#[test]
fn claude_gets_only_wangs_tools() {
    let b = brain(Kind::Claude, "/usr/bin/claude");
    let a = b.argv("tg-42", None, "SYSTEM", "hello");
    let at = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].clone());
    assert_eq!(at("--tools").as_deref(), Some(""), "built-in tools off");
    assert_eq!(at("--allowedTools").as_deref(), Some("mcp__wang__*"));
    assert_eq!(at("--setting-sources").as_deref(), Some(""), "not the user's hooks/plugins");
    assert!(a.contains(&"--strict-mcp-config".to_string()));
    let mcp: Value = serde_json::from_str(&at("--mcp-config").unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["wang"]["command"], "/opt/wang");
    let args: Vec<&str> = mcp["mcpServers"]["wang"]["args"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(args, ["mcp", "--via-daemon", "--session", "tg-42", "--socket", "/run/user/1/wang.sock"]);
    assert_eq!(at("-p").as_deref(), Some("hello"));
    assert_eq!(at("--append-system-prompt").as_deref(), Some("SYSTEM"));
    assert!(at("--resume").is_none());
    let a = b.argv("tg-42", Some("sess-1"), "SYSTEM", "again");
    assert_eq!(a.iter().position(|x| x == "--resume").map(|i| a[i + 1].as_str()), Some("sess-1"));

    // Jailed: the proxy and socket are at their in-jail paths, and the jail sees no user files.
    let mut j = brain(Kind::Claude, "/usr/bin/claude");
    let Ok(jail) = wang_jail::Jail::new() else { return };
    j.jail = Some(Arc::new(jail));
    let mcp: Value = serde_json::from_str(&{
        let a = j.argv("s", None, "", "x");
        a[a.iter().position(|x| x == "--mcp-config").unwrap() + 1].clone()
    })
    .unwrap();
    assert_eq!(mcp["mcpServers"]["wang"]["command"], JAIL_WANG);
}

#[test]
fn jail_policy_shows_only_the_cli_state() {
    let b = brain(Kind::Codex, "/usr/bin/codex");
    let p = b.policy();
    let home = dirs::home_dir().unwrap();
    assert!(p.write.is_empty(), "nothing of yours is writable");
    assert!(!p.read.iter().any(|r| r == &home), "home isn't visible");
    assert!(p.domains.iter().any(|d| d == "chatgpt.com") && !p.domains.iter().any(|d| d == "*"));
    assert!(p.binds.iter().any(|b| b.dst == Path::new(JAIL_SOCKET)));
}

#[test]
fn codex_argv() {
    let b = brain(Kind::Codex, "/usr/bin/codex");
    let a = b.argv("tg-1", None, "RULES", "do it");
    assert_eq!(&a[..2], ["/usr/bin/codex", "exec"]);
    assert!(a.windows(2).any(|w| w == ["-s", "read-only"]));
    assert!(a.iter().any(|x| x == r#"mcp_servers.wang.command="/opt/wang""#));
    assert!(a.iter().any(|x| x.starts_with("mcp_servers.wang.args=[\"mcp\",\"--via-daemon\"")));
    assert!(a.last().unwrap().contains("RULES") && a.last().unwrap().ends_with("do it"));
    let a = b.argv("tg-1", Some("th-9"), "RULES", "more");
    assert_eq!(&a[..4], ["/usr/bin/codex", "exec", "resume", "th-9"]);
    assert_eq!(a.last().unwrap(), "more", "instructions only on the first message");
}

/// A stand-in for `claude -p`: makes one tool call through the daemon socket (like the MCP
/// proxy would), then streams a reply that quotes the tool result.
const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, socket, sys
argv = sys.argv[1:]
mcp = json.loads(argv[argv.index("--mcp-config") + 1])
args = mcp["mcpServers"]["wang"]["args"]
session = args[args.index("--session") + 1]
resumed = "--resume" in argv
assert argv[argv.index("--tools") + 1] == ""
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["WANG_SOCKET"])
s.sendall((json.dumps({"type": "tool_call", "session": session, "name": "shell", "input": {"command": "echo via-brain"}}) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    buf += s.recv(65536)
res = json.loads(buf)
def out(v): print(json.dumps(v), flush=True)
out({"type": "system", "subtype": "init", "session_id": "S-" + session})
out({"type": "stream_event", "parent_tool_use_id": None, "event": {"type": "message_start"}})
out({"type": "stream_event", "parent_tool_use_id": None, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": ("again: " if resumed else "") + "tool said " + res["text"].strip() + (" (error)" if res["is_error"] else "")}}})
out({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour", "resetsAt": 1794965400, "unifiedWindows": {"five_hour": {"utilization": 0.7}}}})
out({"type": "result", "subtype": "success", "is_error": False, "result": "ignored", "total_cost_usd": 0.01, "usage": {"input_tokens": 5, "output_tokens": 3}})
"#;

#[tokio::test(flavor = "current_thread")]
async fn a_brain_turn_runs_tools_in_the_daemon_with_approvals() {
    if which("python3").is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("wang-brain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sock = dir.join("wang.sock");

    let mut b = brain(Kind::Claude, script.to_str().unwrap());
    b.socket = sock.clone();
    // ToolCtx::new has the sandbox off, so every shell call needs approval.
    let appr = Arc::new(Approvals::new(None, Duration::from_secs(10)));
    let a = agent(Mock::new(vec![])).with_approvals(Some(appr.clone())).with_brain(Some(Arc::new(b)));
    let hub = Arc::new(Hub::new(a));
    let ch = UnixSocketChannel::bind(sock.clone()).await.unwrap();
    let server = tokio::spawn(ch.serve(hub.clone()));

    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
    let emit = move |m: ServerMsg| {
        let _ = etx.send(m);
    };
    // The phone: approve whatever is asked.
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let watcher = {
        let (appr, seen) = (appr.clone(), seen.clone());
        tokio::spawn(async move {
            while let Some(m) = erx.recv().await {
                if let ServerMsg::ApprovalRequest { id, detail, .. } = &m {
                    assert!(detail.contains("echo via-brain"), "{detail}");
                    appr.resolve(id, true);
                }
                seen.lock().unwrap().push(m);
            }
        })
    };
    let conv = hub.session("tg-42").await;
    let r = {
        let mut c = conv.lock().await;
        hub.agent.run_turn(&mut c, "telegram", "run something", &emit).await.unwrap()
    };
    assert!(r.reply.starts_with("tool said exit code: 0") && r.reply.ends_with("via-brain"), "{}", r.reply);
    assert_eq!(r.calls, 1);
    assert_eq!(r.cost_usd, 0.0, "subscription: nothing billed per token");
    let footer = r.footer();
    assert!(footer.contains("via your Claude subscription") && footer.contains("five-hour limit 70% used"), "{footer}");
    assert_eq!(conv.lock().await.cli_session.as_deref(), Some("claude:S-tg-42"));
    {
        let s = seen.lock().unwrap();
        assert!(s.iter().any(|m| matches!(m, ServerMsg::ToolStart { name, .. } if name == "shell")));
        assert!(s.iter().any(|m| matches!(m, ServerMsg::ApprovalResult { decision, .. } if decision == "approved")));
        assert!(s.iter().any(|m| matches!(m, ServerMsg::ToolEnd { ok: true, .. })));
    }
    // Traced like any task: the tool call sits under the brain's task.
    let trace = wang_trace::render(&hub.agent.tracer.spans(r.task_id).unwrap());
    assert!(trace.contains("tool shell") && trace.contains("echo via-brain"), "{trace}");

    // Next message resumes the CLI session.
    let r2 = {
        let mut c = conv.lock().await;
        hub.agent.run_turn(&mut c, "telegram", "and again", &emit).await.unwrap()
    };
    assert!(r2.reply.starts_with("again: "), "{}", r2.reply);

    drop(watcher);
    server.abort();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn a_used_up_subscription_falls_back_to_the_api_model() {
    if which("python3").is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("wang-brain-limit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nimport json\nprint(json.dumps({\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"five_hour\",\"resetsAt\":1794965400}}))\nprint(json.dumps({\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":True,\"result\":\"\"}))\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let a = agent(Mock::new(vec![crate::model::testing::resp(json!([{"type": "text", "text": "API answer."}]), "end_turn", 0)]))
        .with_brain(Some(Arc::new(brain(Kind::Claude, script.to_str().unwrap()))));
    let mut conv = Conversation::new("t");
    let said = std::sync::Mutex::new(String::new());
    let emit = |m: ServerMsg| {
        if let ServerMsg::Text { delta } = m {
            said.lock().unwrap().push_str(&delta);
        }
    };
    let r = a.run_turn(&mut conv, "telegram", "hi", &emit).await.unwrap();
    assert_eq!(r.reply, "API answer.");
    let said = said.into_inner().unwrap();
    assert!(said.contains("usage limit is used up") && said.contains("instead"), "{said}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn switching_brain_rewrites_only_the_provider() {
    let dir = std::env::temp_dir().join(format!("wang-setprov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.toml");
    std::fs::write(&p, "# mine\n[model]\nprovider = \"openrouter\"   # keep\nmodel = \"anthropic/claude-haiku-5.5\"\nmax_tokens = 9\n").unwrap();
    crate::config::set_provider(&p, "claude-code").unwrap();
    let s = std::fs::read_to_string(&p).unwrap();
    let c: crate::config::Config = toml::from_str(&s).unwrap();
    assert_eq!(c.model.provider, "claude-code");
    assert_eq!(c.model.model, None, "an OpenRouter model id means nothing to Claude Code");
    assert_eq!(c.model.max_tokens, 9);
    assert!(s.contains("# mine") && s.contains("# keep"), "{s}");
    crate::config::set_provider(&p, "codex").unwrap();
    assert!(std::fs::read_to_string(&p).unwrap().contains("provider = \"codex\""));
    std::fs::remove_dir_all(&dir).ok();
}
