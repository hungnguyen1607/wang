//! wang-proxy: put wang's token savings and cost ledger in front of any agent.
//!
//! A local HTTP proxy for the two APIs coding agents speak:
//! - **Anthropic Messages** (`/v1/messages`): Claude Code (`ANTHROPIC_BASE_URL=http://127.0.0.1:8788`).
//! - **OpenAI-compatible chat completions** (`/openai/v1/chat/completions` → OpenRouter by
//!   default): opencode and most other agents.
//!
//! For each request it trims huge tool outputs, replaces repeated ones, and adds a cache hint
//! when the client set none ([`transform`]). Responses stream through untouched while usage is
//! read off the stream ([`usage`]) into wang's ledger, so `wang cost` covers these agents too and
//! `wang proxy stats` shows what the rewrites saved.
//!
//! It keeps no API keys: the client's own auth headers are forwarded. It listens on loopback only.

pub mod transform;
pub mod usage;

use std::collections::HashMap;
use std::convert::Infallible;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::{info, warn};
use wang_tokens::{CallRecord, Ledger, ProxyRecord};

pub use transform::{Edits, Rules};
pub use usage::{Api, Tap};

/// Largest request body accepted (a 1M-token conversation with images fits).
const MAX_BODY: usize = 64 << 20;
/// Non-streamed responses are buffered up to this to read their usage.
const MAX_TAPPED_JSON: usize = 8 << 20;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Base for Anthropic-style paths (`/v1/...` is appended).
    pub anthropic_upstream: String,
    /// Base for OpenAI-style paths (`/chat/completions` is appended).
    pub openai_upstream: String,
    pub rules: Rules,
    /// Refuse new model calls once today's proxied spend reaches this.
    pub daily_budget_usd: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8788)),
            anthropic_upstream: "https://api.anthropic.com".into(),
            openai_upstream: "https://openrouter.ai/api/v1".into(),
            rules: Rules::default(),
            daily_budget_usd: None,
        }
    }
}

type Body = BoxBody<Bytes, std::io::Error>;

pub struct Proxy {
    cfg: Config,
    http: reqwest::Client,
    ledger: Arc<Ledger>,
    /// Conversation key → (ledger task id, calls so far).
    convos: Mutex<HashMap<u64, (i64, u32)>>,
}

/// Where a request goes and what the proxy does with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    api: Api,
    url: String,
    /// Rewrite the body (messages / chat completions / count_tokens).
    rewrite: bool,
    /// A model call: meter it and count it against the budget.
    meter: bool,
}

impl Proxy {
    pub fn new(cfg: Config, ledger: Arc<Ledger>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // No overall timeout: long generations stream for minutes. Don't follow redirects
            // with the client's credentials attached.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("wang-proxy/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { cfg, http, ledger, convos: Mutex::new(HashMap::new()) })
    }

    fn route(&self, method: &Method, path_and_query: &str) -> Option<Route> {
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let with_query = |base: &str, p: &str| match query {
            Some(q) => format!("{}{p}?{q}", base.trim_end_matches('/')),
            None => format!("{}{p}", base.trim_end_matches('/')),
        };
        let post = method == Method::POST;
        let openai_path = path
            .strip_prefix("/openai")
            .map(|p| p.strip_prefix("/v1").unwrap_or(p))
            .or_else(|| (path == "/v1/chat/completions").then_some("/chat/completions"));
        if let Some(p) = openai_path {
            let calls = post && p == "/chat/completions";
            return Some(Route { api: Api::OpenAi, url: with_query(&self.cfg.openai_upstream, p), rewrite: calls, meter: calls });
        }
        if path.starts_with("/v1/") {
            let messages = post && path == "/v1/messages";
            let count = post && path == "/v1/messages/count_tokens";
            return Some(Route {
                api: Api::Anthropic,
                url: with_query(&self.cfg.anthropic_upstream, path),
                rewrite: messages || count,
                meter: messages,
            });
        }
        None
    }

    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let started = Instant::now();
        let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        if req.method() == Method::GET && (pq == "/" || pq == "/health") {
            return text(StatusCode::OK, "wang proxy ok\n");
        }
        let Some(route) = self.route(req.method(), &pq) else {
            return text(StatusCode::NOT_FOUND, "wang proxy: unknown path. Anthropic API: /v1/..., OpenAI-compatible: /openai/v1/...\n");
        };
        let client = client_name(req.headers());
        let (parts, body) = req.into_parts();
        let raw = match Limited::new(body, MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => return api_error(route.api, StatusCode::PAYLOAD_TOO_LARGE, &format!("wang proxy: request body: {e}")),
        };

        // Rewrite.
        let mut parsed: Option<Value> = if route.rewrite { serde_json::from_slice(&raw).ok() } else { None };
        let mut edits = Edits::default();
        let mut model = String::new();
        let mut task = None;
        if let Some(body) = parsed.as_mut() {
            model = body["model"].as_str().unwrap_or("").to_string();
            if route.meter {
                task = self.task_for(&client, body);
            }
            edits = match route.api {
                Api::Anthropic => transform::anthropic(body, &self.cfg.rules),
                Api::OpenAi => transform::openai(body, &self.cfg.rules),
            };
        }
        let mut out = match (&parsed, edits.changed()) {
            (Some(v), true) => Bytes::from(serde_json::to_vec(v).expect("serializing JSON")),
            _ => raw.clone(),
        };
        let mut rec = ProxyRecord {
            task_id: task.map(|(id, _)| id),
            client: &client,
            api: route.api.name(),
            model: &model,
            status: 0,
            bytes_in: raw.len(),
            bytes_out: out.len(),
            trimmed: edits.trimmed,
            deduped: edits.deduped,
            cache_hint: edits.cache_hint,
            latency_ms: 0,
        };

        // Budget.
        if route.meter
            && let Some(limit) = self.cfg.daily_budget_usd
        {
            let spent = self.ledger.proxy_cost_today().unwrap_or(0.0);
            if spent >= limit {
                self.log_err(self.ledger.record_proxy(&rec));
                warn!(client, spent, limit, "daily budget reached; refusing model call");
                return api_error(
                    route.api,
                    StatusCode::FORBIDDEN,
                    &format!("wang proxy: today's budget of ${limit:.2} is used up (${spent:.2} spent). Raise --daily-budget or wait until tomorrow."),
                );
            }
        }

        let mut resp = match self.send(&parts.method, &route.url, &parts.headers, out.clone()).await {
            Ok(r) => r,
            Err(e) => {
                rec.latency_ms = started.elapsed().as_millis() as u64;
                self.log_err(self.ledger.record_proxy(&rec));
                return api_error(route.api, StatusCode::BAD_GATEWAY, &format!("wang proxy: upstream unreachable: {e}"));
            }
        };

        // A thinking block bound to a prefix the proxy no longer reproduces (e.g. its rules
        // changed mid-conversation): drop the thinking blocks and retry once.
        if route.api == Api::Anthropic && resp.status() == StatusCode::BAD_REQUEST && parsed.is_some() {
            let headers = resp.headers().clone();
            let err = resp.bytes().await.unwrap_or_default();
            let mismatch = String::from_utf8_lossy(&err).contains("bound to a different conversation");
            let stripped = match (mismatch, parsed.as_mut()) {
                (true, Some(body)) => transform::strip_thinking(body),
                _ => 0,
            };
            if stripped == 0 {
                rec.status = 400;
                rec.latency_ms = started.elapsed().as_millis() as u64;
                self.log_err(self.ledger.record_proxy(&rec));
                return buffered(StatusCode::BAD_REQUEST, &headers, err);
            }
            warn!(client, stripped, "thinking blocks no longer match their conversation; retrying without them");
            out = Bytes::from(serde_json::to_vec(parsed.as_ref().unwrap()).expect("serializing JSON"));
            rec.bytes_out = out.len();
            resp = match self.send(&parts.method, &route.url, &parts.headers, out).await {
                Ok(r) => r,
                Err(e) => return api_error(route.api, StatusCode::BAD_GATEWAY, &format!("wang proxy: upstream unreachable: {e}")),
            };
        }

        let status = resp.status();
        rec.status = status.as_u16();
        let sse = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        let mut builder = Response::builder().status(status);
        for (k, v) in resp.headers() {
            if !hop_by_hop(k) && k != header::CONTENT_LENGTH {
                builder = builder.header(k, v);
            }
        }

        // Stream the body through, reading usage as it passes, and record when it ends.
        let (mut tx, rx) = futures::channel::mpsc::channel::<Bytes>(16);
        let owned = OwnedRecord::from(&rec);
        let this = self.clone();
        let meter = route.meter && status.is_success();
        let api = route.api;
        tokio::spawn(async move {
            let mut tap = Tap::new(api);
            let mut json_buf = Vec::new();
            let mut stream = resp.bytes_stream();
            let mut cut = None;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        cut = Some(format!("upstream stream: {}", e.without_url()));
                        break;
                    }
                };
                if meter {
                    if sse {
                        tap.feed_sse(&chunk);
                    } else if json_buf.len() + chunk.len() <= MAX_TAPPED_JSON {
                        json_buf.extend_from_slice(&chunk);
                    }
                }
                if tx.send(chunk).await.is_err() {
                    cut = Some("client disconnected".into());
                    break;
                }
            }
            if meter && !sse {
                tap.feed_json(&json_buf);
            }
            this.finish(owned, task, tap, meter, started, cut);
        });
        let body = StreamBody::new(rx.map(|b| Ok::<_, std::io::Error>(Frame::data(b))));
        builder.body(BodyExt::boxed(body)).unwrap_or_else(|e| text(StatusCode::BAD_GATEWAY, &format!("wang proxy: {e}\n")))
    }

    async fn send(&self, method: &Method, url: &str, headers: &HeaderMap, body: Bytes) -> reqwest::Result<reqwest::Response> {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            if !hop_by_hop(k) && k != header::HOST && k != header::CONTENT_LENGTH && k != header::ACCEPT_ENCODING {
                h.append(k, v.clone());
            }
        }
        // Responses must arrive uncompressed so usage can be read off them.
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        self.http.request(method.clone(), url).headers(h).body(body).send().await.map_err(|e| e.without_url())
    }

    /// One ledger task per conversation: keyed by client + system prompt + first message.
    fn task_for(&self, client: &str, body: &Value) -> Option<(i64, u32)> {
        let msgs = body["messages"].as_array()?;
        let mut h = DefaultHasher::new();
        client.hash(&mut h);
        body["system"].to_string().hash(&mut h);
        msgs.iter().take(2).for_each(|m| m.to_string().hash(&mut h));
        let key = h.finish();
        let mut convos = self.convos.lock().unwrap();
        if let Some(t) = convos.get(&key) {
            return Some(*t);
        }
        if convos.len() > 10_000 {
            convos.clear();
        }
        let id = self.ledger.start_task(&format!("proxy:{client}"), &preview(msgs)).ok()?;
        let _ = self.ledger.finish_task(id, 0, "proxied");
        convos.insert(key, (id, 0));
        Some((id, 0))
    }

    fn finish(&self, rec: OwnedRecord, task: Option<(i64, u32)>, tap: Tap, meter: bool, started: Instant, cut: Option<String>) {
        let latency_ms = started.elapsed().as_millis() as u64;
        let model = tap.model.clone().unwrap_or_else(|| rec.model.clone());
        let mut r = rec.as_record();
        r.model = &model;
        r.latency_ms = latency_ms;
        self.log_err(self.ledger.record_proxy(&r));
        let mut cost = None;
        if meter
            && tap.saw_usage
            && let Some((task_id, _)) = task
        {
            let call = CallRecord {
                model: &model,
                usage: tap.usage,
                provider_cost: tap.cost,
                latency_ms,
                ttft_ms: tap.first_output.map(|t| t.duration_since(started).as_millis() as u64),
                stop_reason: tap.stop.as_deref().unwrap_or(if cut.is_some() { "cut" } else { "?" }),
            };
            match self.ledger.record_call(task_id, &call) {
                Ok(c) => cost = Some(c),
                Err(e) => warn!("ledger: {e:#}"),
            }
            let steps = {
                let mut convos = self.convos.lock().unwrap();
                let entry = convos.values_mut().find(|(id, _)| *id == task_id);
                entry.map(|e| {
                    e.1 += 1;
                    e.1
                })
            };
            if let Some(steps) = steps {
                self.log_err(self.ledger.finish_task(task_id, steps, "proxied"));
            }
        }
        let u = tap.usage;
        info!(
            client = %rec.client,
            model = %model,
            status = rec.status,
            bytes = %format!("{}→{}", rec.bytes_in, rec.bytes_out),
            trimmed = rec.trimmed,
            deduped = rec.deduped,
            cache_hint = rec.cache_hint,
            prompt = u.prompt_tokens(),
            cached = u.cache_read_tokens,
            output = u.output_tokens,
            cost = %cost.map(|c| format!("${c:.4}")).unwrap_or_default(),
            ms = latency_ms,
            note = %cut.or(tap.error).unwrap_or_default(),
            "request"
        );
    }

    fn log_err<T>(&self, r: Result<T>) {
        if let Err(e) = r {
            warn!("ledger: {e:#}");
        }
    }
}

/// `ProxyRecord` borrows; the response pump outlives the request handler.
struct OwnedRecord {
    task_id: Option<i64>,
    client: String,
    api: &'static str,
    model: String,
    status: u16,
    bytes_in: usize,
    bytes_out: usize,
    trimmed: u32,
    deduped: u32,
    cache_hint: bool,
}

impl OwnedRecord {
    fn from(r: &ProxyRecord) -> Self {
        Self {
            task_id: r.task_id,
            client: r.client.into(),
            api: if r.api == Api::Anthropic.name() { Api::Anthropic.name() } else { Api::OpenAi.name() },
            model: r.model.into(),
            status: r.status,
            bytes_in: r.bytes_in,
            bytes_out: r.bytes_out,
            trimmed: r.trimmed,
            deduped: r.deduped,
            cache_hint: r.cache_hint,
        }
    }

    fn as_record(&self) -> ProxyRecord<'_> {
        ProxyRecord {
            task_id: self.task_id,
            client: &self.client,
            api: self.api,
            model: &self.model,
            status: self.status,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            trimmed: self.trimmed,
            deduped: self.deduped,
            cache_hint: self.cache_hint,
            latency_ms: 0,
        }
    }
}

fn hop_by_hop(k: &HeaderName) -> bool {
    matches!(
        k.as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "proxy-authorization" | "proxy-authenticate" | "te" | "trailer"
            | "transfer-encoding" | "upgrade"
    )
}

/// `claude-code`, `opencode`, or the first word of the user agent.
fn client_name(h: &HeaderMap) -> String {
    let ua = h.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ua.contains("claude-cli") || ua.contains("claude-code") {
        return "claude-code".into();
    }
    if ua.contains("opencode") {
        return "opencode".into();
    }
    let word: String = ua
        .split(['/', ' '])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(24)
        .collect();
    if word.is_empty() { "other".into() } else { word }
}

/// The user's words from the first user message (skipping injected `<system-reminder>`-style blocks).
fn preview(msgs: &[Value]) -> String {
    let first_user = msgs.iter().find(|m| m["role"] == "user");
    let Some(content) = first_user.map(|m| &m["content"]) else { return String::new() };
    let texts: Vec<&str> = match content {
        Value::String(s) => vec![s.as_str()],
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => vec![],
    };
    let pick = texts.iter().rev().find(|t| !t.trim_start().starts_with('<')).or(texts.last()).copied().unwrap_or("");
    pick.trim().chars().take(200).collect()
}

fn text(status: StatusCode, s: &str) -> Response<Body> {
    let body = Full::new(Bytes::from(s.to_string())).map_err(|never| match never {});
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(BodyExt::boxed(body))
        .expect("static response")
}

fn buffered(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response<Body> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        if !hop_by_hop(k) && k != header::CONTENT_LENGTH {
            b = b.header(k, v);
        }
    }
    b.body(BodyExt::boxed(Full::new(body).map_err(|never| match never {}))).expect("copied headers are valid")
}

/// An error in the shape the client's SDK expects, so it shows the message.
fn api_error(api: Api, status: StatusCode, msg: &str) -> Response<Body> {
    let v = match api {
        Api::Anthropic => {
            let kind = match status {
                StatusCode::FORBIDDEN => "permission_error",
                StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
                _ => "api_error",
            };
            json!({"type": "error", "error": {"type": kind, "message": msg}})
        }
        Api::OpenAi => json!({"error": {"message": msg, "code": status.as_u16(), "type": "wang_proxy"}}),
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(BodyExt::boxed(Full::new(Bytes::from(v.to_string())).map_err(|never| match never {})))
        .expect("static response")
}

/// Accept connections until the listener fails.
pub async fn serve(listener: TcpListener, proxy: Arc<Proxy>) -> Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let p = proxy.clone();
                async move { Ok::<_, Infallible>(p.handle(req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                .await;
        });
    }
}

/// wang's ledger + trace database (same file `wang cost` reads).
pub fn default_db_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/wang.db")
}

// ---------------------------------------------------------------------------------------------
// Command line, shared by `wang proxy` and the standalone `wang-proxy` binary.

#[derive(clap::Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<CliCmd>,
    #[command(flatten)]
    pub run: RunArgs,
    /// Ledger database (default: wang's, so `wang cost` includes proxied calls)
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
}

#[derive(clap::Subcommand, Debug)]
pub enum CliCmd {
    /// What the proxy passed through and saved
    Stats {
        #[arg(value_enum, default_value = "today")]
        period: Period,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Period {
    Today,
    Week,
    Month,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Port on 127.0.0.1
    #[arg(long, default_value_t = 8788)]
    pub port: u16,
    /// Anthropic API base (Claude Code)
    #[arg(long, default_value = "https://api.anthropic.com")]
    pub anthropic_upstream: String,
    /// OpenAI-compatible API base (opencode etc.)
    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    pub openai_upstream: String,
    /// Max chars kept per tool result (head + tail); 0 = don't trim
    #[arg(long, default_value_t = Rules::default().cap_chars)]
    pub cap_chars: usize,
    /// Don't replace repeated tool outputs
    #[arg(long)]
    pub no_dedup: bool,
    /// Don't add cache hints to requests that have none
    #[arg(long)]
    pub no_cache_hints: bool,
    /// Refuse model calls once today's proxied spend reaches this many USD
    #[arg(long)]
    pub daily_budget: Option<f64>,
}

impl RunArgs {
    pub fn config(&self) -> Config {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], self.port)),
            anthropic_upstream: self.anthropic_upstream.clone(),
            openai_upstream: self.openai_upstream.clone(),
            rules: Rules { cap_chars: self.cap_chars, dedup: !self.no_dedup, cache_hints: !self.no_cache_hints },
            daily_budget_usd: self.daily_budget,
        }
    }
}

pub async fn main(cli: Cli) -> Result<()> {
    let db = cli.db.clone().unwrap_or_else(default_db_path);
    let ledger = Arc::new(Ledger::open(&db)?);
    if let Some(CliCmd::Stats { period }) = cli.cmd {
        let (days, label) = match period {
            Period::Today => (1, "today"),
            Period::Week => (7, "last 7 days"),
            Period::Month => (30, "last 30 days"),
        };
        print!("{}", ledger.proxy_summary(days, label)?);
        return Ok(());
    }
    let cfg = cli.run.config();
    if cfg.daily_budget_usd.is_some_and(|b| !(b > 0.0)) {
        bail!("--daily-budget must be a positive number of USD");
    }
    let listener = TcpListener::bind(cfg.listen).await.with_context(|| format!("binding {}", cfg.listen))?;
    let addr = listener.local_addr()?;
    eprintln!("wang proxy on http://{addr}  (ledger: {})", db.display());
    eprintln!("  Claude Code:  ANTHROPIC_BASE_URL=http://{addr} claude");
    eprintln!("  opencode etc: OpenAI-compatible base URL http://{addr}/openai/v1  (→ {})", cfg.openai_upstream);
    eprintln!(
        "  rules: trim tool output > {} chars, dedup {}, cache hints {}{}",
        cfg.rules.cap_chars,
        if cfg.rules.dedup { "on" } else { "off" },
        if cfg.rules.cache_hints { "on" } else { "off" },
        cfg.daily_budget_usd.map(|b| format!(", daily budget ${b:.2}")).unwrap_or_default()
    );
    let proxy = Arc::new(Proxy::new(cfg, ledger)?);
    tokio::select! {
        r = serve(listener, proxy) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[cfg(test)]
mod tests;
