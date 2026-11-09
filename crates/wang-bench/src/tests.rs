use super::*;
use serde_json::json;
use wang_daemon::model::openrouter::StreamAcc;

#[test]
fn script_streams_parse_with_the_real_client_parser() {
    let s = Script::default_task();
    let mut acc = StreamAcc::default();
    acc.feed(s.sse(0).as_bytes()).unwrap();
    let r = acc.finish("m").unwrap();
    assert_eq!(r.stop_reason, "tool_use");
    assert_eq!(r.content[0]["name"], "shell");
    assert_eq!(r.content[0]["input"]["command"], "wc -l data.txt");

    let mut acc = StreamAcc::default();
    for piece in s.sse(2).as_bytes().chunks(5) {
        acc.feed(piece).unwrap();
    }
    let r = acc.finish("m").unwrap();
    assert_eq!(r.stop_reason, "end_turn");
    assert!(r.content[0]["text"].as_str().unwrap().contains("3 lines"));
    assert_eq!(r.cost_usd, Some(0.0));
    assert_eq!(r.usage.cache_read_tokens, 2_000);
}

#[test]
fn step_counts_assistant_messages() {
    let body = json!({"messages": [{"role": "system"}, {"role": "user"}, {"role": "assistant"}, {"role": "tool"}, {"role": "assistant"}]});
    assert_eq!(fake::step_of(&body), 2);
    assert_eq!(fake::step_of(&json!({})), 0);
}

#[test]
fn percentiles() {
    let p = pct((1..=100).map(f64::from).collect());
    assert_eq!((p.p50, p.max), (51.0, 100.0));
    assert_eq!(p.p95, 95.0);
    assert_eq!(pct(vec![]).max, 0.0);
}

#[tokio::test(flavor = "current_thread")]
async fn small_ramp_end_to_end_without_sandbox() {
    let out = std::env::temp_dir().join(format!("wang-bench-test-{}", std::process::id()));
    let opts = BenchOpts {
        levels: vec![1, 8],
        tasks_per_agent: 2,
        latency: Duration::from_millis(5),
        min_free_mb: 0,
        sandbox: false,
        out: out.clone(),
        ..Default::default()
    };
    let r = run(&opts, |_| {}).await.unwrap();
    assert_eq!(r.levels.len(), 2, "{r}");
    for l in &r.levels {
        assert_eq!(l.failed, 0, "{:?}", l.first_error);
        assert_eq!(l.tool_errors, 0);
        assert_eq!(l.tasks, l.agents * 2);
        assert!(l.model_ms.p50 > 0.0 && l.tool_ms.p50 > 0.0);
    }
    assert!(r.to_string().contains("most agents at once with no failures: 8"), "{r}");
    assert!(out.join("report.json").exists());
    std::fs::remove_dir_all(out).ok();
}
