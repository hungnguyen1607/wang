use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[test]
fn spoken_part_splits_at_marker() {
    let (spoken, details) = spoken_part("Short answer.\n\n---\nHere are the details:\n- one\n- two");
    assert_eq!(spoken, "Short answer.");
    let d = details.unwrap();
    assert!(d.contains("Here are the details") && d.contains("- two"), "{d}");
}

#[test]
fn spoken_part_falls_back_to_first_sentences() {
    let (spoken, details) = spoken_part("One. Two. Three. Four.");
    assert_eq!(spoken, "One. Two. Three.");
    assert_eq!(details.as_deref(), Some("Four."));
    // No split, single short sentence: nothing left as details.
    let (spoken, details) = spoken_part("Just this.");
    assert_eq!(spoken, "Just this.");
    assert!(details.is_none());
}

#[test]
fn leading_dashes_are_a_rule_not_a_split() {
    let (spoken, details) = spoken_part("---\nA headline.\nNext sentence.");
    assert!(spoken.contains("A headline."), "{spoken}");
    assert!(details.is_none(), "no separator yet: {details:?}");
}

#[test]
fn spoken_part_strips_links_code_and_urls() {
    let (spoken, _) = spoken_part("Check https://example.com now.\n\n---\n`run()` it");
    assert_eq!(spoken, "Check now.");
    let s = clean_speech("see [the docs](https://example.com) and `cargo build` *soon*");
    assert_eq!(s, "see the docs and cargo build soon");
}

#[test]
fn approval_intent_matches_whole_message() {
    for y in ["yes", "YES", "yeah", "approve", "go ahead", "sure", "ok"] {
        assert_eq!(approval_intent(y), Some(true), "{y}");
    }
    for n in ["no", "No.", "nope", "deny", "stop", "cancel"] {
        assert_eq!(approval_intent(n), Some(false), "{n}");
    }
    assert_eq!(approval_intent("what do you think"), None);
    assert_eq!(approval_intent("yes please do it"), None, "only whole-message matches");
    assert_eq!(approval_intent(""), None);
}

/// A tiny HTTP server that records requests and answers, for the speech client.
#[derive(Debug)]
struct Request {
    path: String,
    content_type: String,
    auth: String,
    body: Vec<u8>,
}

async fn serve_once() -> (String, Arc<Mutex<Vec<Request>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match s.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).position(|w| w == b"\r\n\r\n").is_some() {
                            let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                            let len: usize = head
                                .lines()
                                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                                .unwrap_or(0);
                            if buf.len() >= head_end + len {
                                let body = buf[head_end..head_end + len].to_vec();
                                let path = head.split_whitespace().nth(1).unwrap().to_string();
                                let content_type = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string())).unwrap_or_default();
                                let auth = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("authorization:").map(|v| v.trim().to_string())).unwrap_or_default();
                                let is_stt = path.ends_with("/audio/transcriptions");
                                let out = if is_stt { json!({"text": "hello from wang"}).to_string() } else { String::from("OGGDATA") };
                                let ct = if is_stt { "application/json" } else { "audio/ogg" };
                                calls2.lock().unwrap().push(Request { path, content_type, auth, body });
                                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: {ct}\r\ncontent-length: {}\r\n\r\n{out}", out.len());
                                let _ = s.write_all(resp.as_bytes()).await;
                                break;
                            }
                        }
                    }
                }
            }
        }
    });
    (format!("http://{addr}"), calls)
}

fn cfg(base: &str) -> VoiceCfg {
    VoiceCfg { stt_base: base.to_string(), tts_base: base.to_string(), enabled: true, api_key_env: "OPENAI_API_KEY".into(), ..Default::default() }
}

#[tokio::test(flavor = "current_thread")]
async fn cloud_speech_transcribes_and_speaks() {
    let (base, calls) = serve_once().await;
    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-key") };
    let speech = CloudSpeech::new(cfg(&base)).unwrap();

    let t = speech.transcribe(vec![0x4f, 0x67, 0x67], "audio/ogg").await.unwrap();
    assert_eq!(t.text, "hello from wang");

    let ogg = speech.speak("hello there").await.unwrap();
    assert_eq!(ogg, b"OGGDATA");

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    // Transcription: multipart with file, model and language.
    let stt = &calls[0];
    assert!(stt.path.ends_with("/audio/transcriptions"), "{}", stt.path);
    assert!(stt.content_type.starts_with("multipart/form-data"), "{}", stt.content_type);
    assert!(stt.auth.to_ascii_lowercase().contains("bearer sk-test-key"), "{}", stt.auth);
    assert!(stt.body.windows(3).any(|w| w == b"Ogg"), "file bytes present");
    let body = String::from_utf8_lossy(&stt.body);
    assert!(body.contains("gpt-4o-mini-transcribe"), "{body}");
    assert!(body.contains("language"), "{body}");
    // Speech: JSON with opus.
    let tts = &calls[1];
    assert!(tts.path.ends_with("/audio/speech"), "{}", tts.path);
    let body = String::from_utf8_lossy(&tts.body);
    assert!(body.contains("\"response_format\":\"opus\"") || body.contains("\"response_format\": \"opus\""), "{body}");
    assert!(body.contains("alloy"), "{body}");
    assert!(body.contains("hello there"), "{body}");
}

#[tokio::test(flavor = "current_thread")]
async fn speech_errors_never_contain_the_key() {
    unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test-key") };
    // Nothing listens on port 9; the connection error must not echo the key.
    let speech = CloudSpeech::new(cfg("http://127.0.0.1:9")).unwrap();
    let e = speech.speak("hi").await.unwrap_err();
    assert!(!format!("{e:#}").contains("sk-test-key"), "{e:#}");
}
