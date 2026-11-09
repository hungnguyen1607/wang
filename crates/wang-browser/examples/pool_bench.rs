//! Launch N jailed browsers at once; report launch time and memory (idle, after a page).
//! `cargo build --release -p wang-jail --bin jail && cargo run --release -p wang-browser --example pool_bench -- 4 https://news.ycombinator.com`
use wang_browser::*;
use wang_jail::Jail;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let jail = Jail::with_helper("target/release/jail".into()).unwrap();
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let url = std::env::args().nth(2).unwrap_or_else(|| "https://news.ycombinator.com".into());
    let cfg = BrowserCfg::default();
    let avail0 = pool::mem_available().unwrap_or(0) >> 20;
    let t = std::time::Instant::now();
    let launched = futures_join(n, &jail, &cfg).await;
    let all_ms = t.elapsed().as_millis();
    let mut bs: Vec<Browser> = launched.into_iter().map(|r| r.unwrap()).collect();
    let idle: Vec<u64> = bs.iter().map(|b| b.memory_bytes().unwrap_or(0) >> 20).collect();
    let launch: Vec<u64> = bs.iter().map(|b| b.launch_ms as u64).collect();
    for b in bs.iter_mut() { b.open(&url).await.unwrap(); }
    let loaded: Vec<u64> = bs.iter().map(|b| b.memory_bytes().unwrap_or(0) >> 20).collect();
    let avail1 = pool::mem_available().unwrap_or(0) >> 20;
    println!("{n} browsers: all up in {all_ms} ms; launch ms {launch:?}");
    println!("idle MiB {idle:?} (sum {}); after {url}: {loaded:?} (sum {})", idle.iter().sum::<u64>(), loaded.iter().sum::<u64>());
    println!("system MemAvailable {avail0} → {avail1} MiB (−{})", avail0.saturating_sub(avail1));
    for b in bs { b.close().await; }
}

async fn futures_join(n: usize, jail: &Jail, cfg: &BrowserCfg) -> Vec<anyhow::Result<Browser>> {
    let mut set = Vec::new();
    for _ in 0..n { set.push(Browser::launch(jail, cfg)); }
    let mut out = Vec::new();
    // current_thread runtime: poll them together
    let joined = join_all(set).await;
    out.extend(joined);
    out
}

async fn join_all<F: std::future::Future>(fs: Vec<F>) -> Vec<F::Output> {
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = fs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in pinned.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) { std::task::Poll::Ready(v) => out[i] = Some(v), std::task::Poll::Pending => pending = true }
            }
        }
        if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
    }).await;
    out.into_iter().map(|o| o.unwrap()).collect()
}
