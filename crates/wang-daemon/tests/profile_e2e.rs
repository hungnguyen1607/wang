//! Opening an app on a named profile, on the real desktop. Opt-in (it opens a window):
//! `WANG_E2E_PROFILE=swe cargo test -p wang-daemon --test profile_e2e -- --nocapture`

use std::sync::Arc;

use serde_json::json;
use wang_daemon::tools::{ToolCtx, run_in};

fn pids(name: &str) -> Vec<String> {
    let o = std::process::Command::new("pgrep").args(["-x", name]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).split_whitespace().map(String::from).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn firefox_on_a_profile() {
    let Ok(profile) = std::env::var("WANG_E2E_PROFILE") else {
        eprintln!("skipped (set WANG_E2E_PROFILE to a Firefox profile name)");
        return;
    };
    let before = pids("firefox");
    let dir = std::env::temp_dir().join(format!("wang-profile-e2e-{}", std::process::id()));
    let ctx = ToolCtx::new(std::env::temp_dir()).with_desktop(Some(Arc::new(wang_desktop::Desktop::new(dir.clone()))));

    let apps = run_in("desktop", &json!({"action": "apps", "query": "firefox"}), &ctx, "e2e").await;
    eprintln!("{}", apps.text);
    assert!(apps.text.contains("Firefox profiles:") && apps.text.contains(&profile), "{}", apps.text);

    let o = run_in("desktop", &json!({"action": "open", "target": "firefox", "profile": profile}), &ctx, "e2e").await;
    eprintln!("{}", o.text);
    let new: Vec<String> = pids("firefox").into_iter().filter(|p| !before.contains(p)).collect();
    // Close only what this test started.
    for p in &new {
        let _ = std::process::Command::new("kill").arg(p).status();
    }
    std::fs::remove_dir_all(dir).ok();
    assert!(!o.is_error, "{}", o.text);
    assert!(o.text.contains(&format!("on {profile}")), "{}", o.text);
    assert!(!new.is_empty(), "a new Firefox started for that profile");
}
