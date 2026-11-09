//! Real Chrome in the real jail against a local fixture site. Skipped when the machine has no
//! sandbox, no Chrome, or the `jail` helper binary hasn't been built (`cargo build -p wang-jail`).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wang_jail::Jail;

use crate::*;

const INDEX: &str = r#"<!doctype html><html><head><title>Fixture shop</title>
<style>body { font-family: sans-serif } .x { color: red }</style>
<script>window.secretScript = "SCRIPT_TEXT_SHOULD_NOT_SHOW";</script></head>
<body>
<nav><a href="/">Home</a> <a href="/item">Items</a></nav>
<h1>Welcome to the shop</h1>
<p>We sell <b>tea</b> and <a href="/item">fine cups</a>.</p>
<div style="display:none">HIDDEN_TEXT_SHOULD_NOT_SHOW</div>
<form action="/search" method="get">
  <label for="q">Search</label> <input id="q" name="q" placeholder="what?">
  <select name="sort"><option>Newest</option><option value="cheap">Cheapest</option></select>
  <label><input type="checkbox" name="stock"> In stock only</label>
  <button type="submit">Go</button>
</form>
<button id="js" onclick="document.getElementById('out').textContent='clicked!'">Do JS thing</button>
<p id="out">not yet</p>
<table><tr><th>Item</th><th>Price</th></tr><tr><td>Green tea</td><td>$4</td></tr></table>
<main style="display:contents"><p>CONTENTS_TEXT is visible</p></main>
<fancy-card><span>SLOTTED_TEXT inside a component</span></fancy-card>
<script>customElements.define("fancy-card", class extends HTMLElement { constructor() { super(); this.attachShadow({mode: "open"}).innerHTML = "<div><b>SHADOW_TEXT</b> <slot></slot></div>"; } });</script>
<p>See <a href="https://www.rust-lang.org/learn">the Rust site</a>.</p>
<footer><a href="/">Home</a> <a href="/item">Items</a></footer>
</body></html>"#;

const ITEM: &str = r#"<!doctype html><title>Item page</title><h1>A fine cup</h1><a href="/">back home</a>"#;
const ALERT: &str = r#"<!doctype html><title>Alerty</title><script>alert("hi"); confirm("sure?");</script><p>after alerts</p>"#;

async fn fixture() -> (u16, tokio::task::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let h = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split(' ').nth(1).unwrap_or("/").to_string();
                let body = if path.starts_with("/search") {
                    let q = path.split("q=").nth(1).unwrap_or("").split('&').next().unwrap_or("").replace('+', " ");
                    format!("<!doctype html><title>Results</title><h1>Results for {q}</h1><p>sorted: {}</p>", path.contains("sort=cheap"))
                } else if path == "/item" {
                    ITEM.to_string()
                } else if path == "/alert" {
                    ALERT.to_string()
                } else if path == "/big" {
                    let rows: String = (0..3000).map(|i| format!("<p>Row number {i} with some filler text to make it longer</p>")).collect();
                    format!("<!doctype html><title>Big</title>{rows}")
                } else if path == "/hog" {
                    // Allocates until the browser's memory cap stops it.
                    "<!doctype html><title>Hog</title><script>const a=[];for(;;){a.push(new Array(1e6).fill(Math.random()));}</script>".to_string()
                } else {
                    INDEX.to_string()
                };
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (port, h)
}

fn jail() -> Option<Arc<Jail>> {
    let helper = std::env::current_exe().ok()?.parent()?.parent()?.join("jail");
    if !helper.exists() || find_chrome().is_err() {
        eprintln!("skipping: needs {} and a Chrome", helper.display());
        return None;
    }
    Jail::with_helper(helper).ok().map(Arc::new)
}

#[tokio::test(flavor = "current_thread")]
async fn browse_click_type_select_back_in_the_jail() {
    let Some(jail) = jail() else { return };
    let (port, _srv) = fixture().await;
    let base = format!("http://127.0.0.1:{port}");
    let cfg = BrowserCfg { domains: vec![format!("127.0.0.1:{port}")], ..Default::default() };
    let mut b = Browser::launch(&jail, &cfg).await.unwrap();
    assert!(b.launch_ms > 0.0);

    b.open(&format!("{base}/")).await.unwrap();
    let v = b.view(cfg.view_chars).await.unwrap();
    let t = &v.text;
    println!("{t}\n-- {} refs, html {} bytes, view {} bytes", v.refs, v.html_bytes, t.len());
    assert!(t.starts_with("Fixture shop\nhttp://127.0.0.1:"), "{t}");
    assert!(t.contains("# Welcome to the shop"), "{t}");
    assert!(t.contains("We sell tea and [3 fine cups]."), "{t}");
    assert!(t.contains("[4 input text \"Search\"]") && !t.contains("Search [4"), "{t}");
    assert!(t.contains("select = Newest (Newest | Cheapest)"), "{t}");
    assert!(t.contains("checkbox ☐ In stock only]") && !t.contains("In stock only] In stock"), "{t}");
    assert!(t.contains("Item | Price") && t.contains("Green tea | $4"), "{t}");
    assert!(t.contains("CONTENTS_TEXT is visible"), "display:contents content missing:\n{t}");
    assert!(t.contains("SHADOW_TEXT") && t.contains("SLOTTED_TEXT inside a component"), "web component content missing:\n{t}");
    assert!(t.contains("the Rust site → rust-lang.org]"), "{t}");
    for hidden in ["HIDDEN_TEXT", "SCRIPT_TEXT", "font-family"] {
        assert!(!t.contains(hidden), "{hidden} leaked into the view:\n{t}");
    }
    assert!(t.len() < v.html_bytes, "view should be smaller than the HTML");

    let refnum = |t: &str, needle: &str| -> usize {
        let i = t.find(needle).unwrap_or_else(|| panic!("{needle} not in view:\n{t}"));
        let open = t[..i].rfind('[').unwrap();
        t[open + 1..].split(' ').next().unwrap().parse().unwrap()
    };

    // JS click without navigation.
    b.click(refnum(t, "button: Do JS thing")).await.unwrap();
    assert!(b.view(cfg.view_chars).await.unwrap().text.contains("clicked!"));

    // Select + type + submit = a real form navigation.
    let t = b.view(cfg.view_chars).await.unwrap().text;
    assert_eq!(b.select(refnum(&t, "select = Newest"), "cheap").await.unwrap(), "Cheapest");
    b.type_text(refnum(&t, "input text \"Search\""), "green tea", true).await.unwrap();
    let r = b.view(cfg.view_chars).await.unwrap().text;
    assert!(r.contains("# Results for green tea") && r.contains("sorted: true"), "{r}");

    // Back, then follow a link by clicking.
    b.back().await.unwrap();
    let t = b.view(cfg.view_chars).await.unwrap().text;
    assert!(t.contains("Welcome to the shop"), "{t}");
    b.click(refnum(&t, "fine cups")).await.unwrap();
    assert!(b.url().await.unwrap().ends_with("/item"));
    // Stale refs from an old page are refused, not misclicked.
    assert!(b.click(999).await.unwrap_err().to_string().contains("not on the page"));

    // Dialogs don't hang the page.
    b.open(&format!("{base}/alert")).await.unwrap();
    assert!(b.view(cfg.view_chars).await.unwrap().text.contains("after alerts"));

    // Big page: the view is cut and can be paged.
    b.open(&format!("{base}/big")).await.unwrap();
    let big = b.view(200_000).await.unwrap();
    let (p1, parts) = view_part(&big.text, 1, cfg.view_chars);
    assert!(parts > 5 && p1.len() <= cfg.view_chars + 200, "{parts} parts");

    // Allow-list: another local port and a public host are blocked by the jail.
    let other = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let e = b.open(&format!("http://127.0.0.1:{other}/")).await.unwrap_err().to_string();
    assert!(e.contains(&format!("blocked 127.0.0.1:{other}")), "{e}");
    let e = b.open("https://example.com/").await.unwrap_err().to_string();
    assert!(e.contains("example.com"), "{e}");
    for bad in ["file:///etc/passwd", "chrome://version"] {
        assert!(b.open(bad).await.is_err(), "{bad}");
    }
    let stopped = b.close().await;
    assert!(stopped.usage.peak_memory_bytes.unwrap_or(0) > 0);
}

#[tokio::test(flavor = "current_thread")]
async fn memory_cap_kills_a_hog_and_the_browser_recovers() {
    let Some(jail) = jail() else { return };
    let (port, _srv) = fixture().await;
    let cfg = BrowserCfg {
        domains: vec![format!("127.0.0.1:{port}")],
        memory_bytes: 512 << 20,
        nav_timeout: Duration::from_secs(20),
        ..Default::default()
    };
    let mut b = Browser::launch(&jail, &cfg).await.unwrap();
    let e = b.open(&format!("http://127.0.0.1:{port}/hog")).await;
    let after = b.view(cfg.view_chars).await;
    // Either the navigation or the next action reports the crash; memory stayed under the cap.
    let msg = format!("{:?} / {:?}", e.as_ref().err(), after.as_ref().err());
    assert!(e.is_err() || after.is_err(), "hog page didn't fail: {msg}");
    assert!(msg.contains("crash") || msg.contains("exited") || msg.contains("timed out"), "{msg}");
    assert!(b.memory_bytes().unwrap_or(0) <= cfg.memory_bytes, "over the cap");
    // A fresh tab still works (or the browser is reported dead, and the pool would replace it).
    if b.alive() {
        b.open(&format!("http://127.0.0.1:{port}/item")).await.unwrap();
        assert!(b.view(cfg.view_chars).await.unwrap().text.contains("A fine cup"));
    }
    b.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn pool_caps_reuses_and_evicts() {
    let Some(jail) = jail() else { return };
    let (port, _srv) = fixture().await;
    let cfg = BrowserCfg { domains: vec![format!("127.0.0.1:{port}")], max_browsers: 2, min_free_bytes: 0, ..Default::default() };
    let pool = Pool::new(jail, cfg);
    let a = pool.get("a").await.unwrap();
    a.lock().await.open(&format!("http://127.0.0.1:{port}/item")).await.unwrap();
    let a2 = pool.get("a").await.unwrap();
    assert!(Arc::ptr_eq(&a, &a2), "same session reuses its browser");
    drop((a, a2));
    let b = pool.get("b").await.unwrap();
    // Full and "b" is in use: "a" (idle) is evicted for "c".
    let _c = pool.get("c").await.unwrap();
    assert_eq!(pool.launches(), 3);
    let sessions: Vec<String> = pool.status().await.into_iter().map(|s| s.session).collect();
    assert_eq!(sessions, ["b", "c"]);
    // Both remaining are busy: a fourth is refused rather than going over the cap.
    let e = pool.get("d").await.err().expect("should be refused").to_string();
    assert!(e.contains("busy"), "{e}");
    drop(b);
    drop(_c);
    pool.shutdown().await;
}
