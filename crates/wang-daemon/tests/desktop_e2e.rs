//! The desktop tool on the real GNOME session: opens Calculator, computes 2+2 by clicking,
//! reads the answer, checks the guardrails. Opt-in (it opens a window):
//! `WANG_E2E_DESKTOP=1 cargo test -p wang-daemon --test desktop_e2e -- --nocapture`

use std::sync::Arc;

use serde_json::json;
use wang_daemon::tools::{ToolCtx, run_in};

fn calc_pids() -> Vec<String> {
    let o = std::process::Command::new("pgrep").args(["-x", "gnome-calculato"]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn calculator_two_plus_two() {
    if std::env::var("WANG_E2E_DESKTOP").is_err() || wang_desktop::Desktop::available().is_err() {
        eprintln!("skipped (set WANG_E2E_DESKTOP=1 in a desktop session)");
        return;
    }
    let before = calc_pids();
    let dir = std::env::temp_dir().join(format!("wang-desktop-e2e-{}", std::process::id()));
    let ctx = ToolCtx::new(std::env::temp_dir()).with_desktop(Some(Arc::new(wang_desktop::Desktop::new(dir.clone()))));
    let call = |input: serde_json::Value| {
        let ctx = &ctx;
        async move { run_in("desktop", &input, ctx, "e2e").await }
    };

    let o = call(json!({"action": "open", "target": "Calculator"})).await;
    assert!(!o.is_error, "{}", o.text);
    let n: u64 = o.text.split("window ").nth(1).and_then(|r| r.split(':').next()).and_then(|x| x.trim().parse().ok()).expect(&o.text);
    let v = call(json!({"action": "view", "window": n})).await.text;
    let find = |label: &str| -> u64 {
        v.lines().find(|l| l.contains(&format!("] button \"{label}\""))).and_then(|l| l.split('[').nth(1)?.split(']').next()?.parse().ok()).unwrap_or_else(|| panic!("no {label} in\n{v}"))
    };
    for b in ["2", "+", "2", "="] {
        let r = find(b);
        let input = json!({"action": "click", "ref": r});
        assert_eq!(ctx.approval_reason("desktop", &input, "e2e").await, None, "calculator buttons are free");
        let out = call(input).await;
        assert!(!out.is_error, "{}", out.text);
    }
    // The app updates its display on its own main loop.
    let mut after = String::new();
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        after = call(json!({"action": "view", "window": n})).await.text;
        if after.contains("value=\"4\"") {
            break;
        }
    }
    let ok = after.contains("value=\"4\"");
    if !ok {
        for p in calc_pids().into_iter().filter(|p| !before.contains(p)) {
            let _ = std::process::Command::new("kill").arg(p).status();
        }
    }
    assert!(ok, "{}", after.lines().take(8).collect::<Vec<_>>().join("\n"));

    // Guardrails.
    assert!(ctx.approval_reason("desktop", &json!({"action": "click_at", "x": 10, "y": 10}), "e2e").await.is_some());
    assert!(ctx.approval_reason("desktop", &json!({"action": "key", "keys": "ctrl+q"}), "e2e").await.is_some());
    let k = call(json!({"action": "key", "keys": "Escape"})).await;
    assert!(k.is_error && k.text.contains("wang setup screen"), "no portal permission in this test dir: {}", k.text);

    // Close only the calculator this test opened.
    for p in calc_pids().into_iter().filter(|p| !before.contains(p)) {
        let _ = std::process::Command::new("kill").arg(p).status();
    }
    std::fs::remove_dir_all(dir).ok();
}
