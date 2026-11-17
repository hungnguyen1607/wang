//! End to end: `wang eval` arms against the fake model server ($0), recorded live, then replayed.
//! Its own test binary, so setting the API key env var can't race another test.

use std::time::Duration;

use wang_bench::fake::{FakeServer, Script};
use wang_daemon::config::Config;
use wang_evals::{Arm, Mode, RunOpts};

#[tokio::test(flavor = "current_thread")]
async fn arms_record_then_replay() {
    let server = FakeServer::start(Script::default_task(), Duration::ZERO).await.unwrap();
    // SAFETY: the only test in this binary; nothing else reads the environment concurrently.
    unsafe { std::env::set_var("OPENROUTER_API_KEY", "fake") };
    let mut cfg = Config::default();
    cfg.model.base_url = Some(server.base_url());
    cfg.model.model = Some("anthropic/fake".into());

    let root = std::env::temp_dir().join(format!("wang-eval-arms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let suite = root.join("tasks");
    std::fs::create_dir_all(&suite).unwrap();
    std::fs::write(
        suite.join("lines.toml"),
        "id = \"lines\"\nprompt = \"How many lines are in data.txt?\"\n[files]\n\"data.txt\" = \"alpha\\nbeta\\ngamma\\n\"\n[check]\nreply_contains = [\"3 lines\"]\n",
    )
    .unwrap();
    let opts = |mode, out: &str, max_usd| RunOpts {
        mode,
        suite: suite.clone(),
        cassettes: root.join("cassettes"),
        out: root.join(out),
        arms: vec![Arm::Wang, Arm::Plain],
        trials: 2,
        max_usd,
        ..Default::default()
    };

    let live = wang_evals::run(cfg.clone(), &opts(Mode::Live, "live", None)).await.unwrap();
    assert_eq!(live.results.len(), 4, "{live}");
    assert!(live.results.iter().all(|r| r.passed), "{live}");
    for (arm, trial) in [("wang", 1), ("wang", 2), ("plain", 1), ("plain", 2)] {
        assert!(root.join(format!("cassettes/lines.{arm}.t{trial}.json")).exists());
    }
    let text = live.to_string();
    assert!(text.contains("plain vs") || text.contains("wang vs plain"), "{text}");

    let replay = wang_evals::run(cfg.clone(), &opts(Mode::Replay, "replay", None)).await.unwrap();
    assert!(replay.results.iter().all(|r| r.passed && r.diverged == 0), "{replay}");

    // A spent cap stops new live runs before they start.
    let capped = wang_evals::run(cfg, &opts(Mode::Live, "capped", Some(0.0))).await.unwrap();
    assert!(capped.results.iter().all(|r| r.outcome == "over_budget"), "{capped}");
    std::fs::remove_dir_all(root).ok();
}
