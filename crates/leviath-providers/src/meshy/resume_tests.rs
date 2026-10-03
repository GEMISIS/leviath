//! A Meshy call made again after a restart polls the tasks it submitted
//! before, so each task is paid for once.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Duration;

use super::MeshyProvider;
use crate::jobs::JobLog;
use crate::provider::{
    ContentBlock, InferenceRequest, Message, MessageContent, Provider, ProviderError,
};

/// What the mock Meshy saw, and how it answers.
#[derive(Default)]
struct Seen {
    /// Each create, as `POST <path>`.
    creates: Vec<String>,
    /// Each poll, as the task id it asked about.
    polls: Vec<String>,
    /// Whether tasks have finished; until then every poll says `IN_PROGRESS`.
    finished: bool,
    /// Task ids it no longer has, answered 404.
    gone: HashSet<String>,
    /// Task ids that failed.
    failed: HashSet<String>,
}

/// A Meshy-style server: a POST creates `task-<n>`, a GET of a task reports
/// it, and a GET of `/m.glb` downloads the mesh.
async fn mock_meshy() -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (state, glb) = (seen.clone(), format!("{base}/m.glb"));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 65536];
            let read = socket.read(&mut buf).await.unwrap_or(0);
            if read == 0 {
                // A call stopped before it sent anything.
                continue;
            }
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let line = request.lines().next().unwrap_or_default().to_string();
            let mut words = line.split(' ');
            let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let (status, body) = {
                let mut seen = state.lock().unwrap();
                match (method, path) {
                    ("GET", "/m.glb") => ("200 OK", b"glTF-bytes".to_vec()),
                    ("POST", _) => {
                        seen.creates.push(format!("POST {path}"));
                        let id = format!("task-{}", seen.creates.len());
                        ("200 OK", json!({ "result": id }).to_string().into_bytes())
                    }
                    _ => {
                        let id = path.rsplit('/').next().unwrap_or_default().to_string();
                        seen.polls.push(id.clone());
                        let task = match seen.finished {
                            _ if seen.failed.contains(&id) => {
                                json!({ "status": "FAILED", "task_error": { "message": "bad mesh" } })
                            }
                            true => {
                                json!({ "status": "SUCCEEDED", "model_urls": { "glb": glb }, "result": { "rigged_character_glb_url": glb, "animation_glb_url": glb } })
                            }
                            false => json!({ "status": "IN_PROGRESS", "progress": 40 }),
                        };
                        match seen.gone.contains(&id) {
                            true => ("404 Not Found", b"{\"message\":\"no such task\"}".to_vec()),
                            false => ("200 OK", task.to_string().into_bytes()),
                        }
                    }
                }
            };
            let mut response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        }
    });
    (base, seen)
}

fn provider(base: &str) -> MeshyProvider {
    MeshyProvider::new(reqwest::Client::new(), "test-key".into())
        .with_base_url(Some(base.into()))
        .with_poll_interval(Duration::from_millis(1))
}

/// A stored part of type `mime`, with its bytes in place.
fn part(mime: &str, name: &str) -> ContentBlock {
    ContentBlock::Mime {
        part: leviath_core::mime::BlobRef {
            sha256: "b".repeat(64),
            mime_type: leviath_core::mime::MimeType::parse(mime).unwrap(),
            size: 3,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 1,
            stand_in: format!("[{name}]"),
        },
        data: "QUJD".into(),
        name: Some(name.into()),
        deliver: None,
        remote: None,
    }
}

/// A call to `model` with a prompt, an image and a mesh, which every
/// operation here reads what it needs from.
fn request(model: &str) -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "a brass robot".into(),
                },
                part("image/png", "front.png"),
                part("model/gltf-binary", "robot.glb"),
            ]),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.into(),
        max_tokens: 0,
        temperature: 0.0,
        tools: Vec::new(),
        extra: Value::Null,
        request_timeout_secs: None,
    }
}

/// Make the call for a while, with every task still running, then stop it
/// there, as a daemon that dies mid-poll does. Returns what a run file would
/// hold of its log.
async fn stop_mid_poll(
    meshy: &MeshyProvider,
    model: &str,
) -> std::collections::BTreeMap<String, String> {
    let log = JobLog::default();
    let req = request(model);
    // Stopped once it has recorded its task and polled it for a while.
    let recorded = async {
        while log.jobs().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    tokio::select! {
        _ = log.scope(meshy.infer(&req)) => panic!("the call ended while its task still ran"),
        _ = recorded => {}
    }
    log.jobs()
}

/// A restart between a task's submission and its end polls the same task:
/// one submit in all, and the mesh comes back.
#[tokio::test]
async fn a_restart_mid_poll_polls_the_same_task_and_submits_once() {
    let (base, seen) = mock_meshy().await;
    let meshy = provider(&base);
    let recorded = stop_mid_poll(&meshy, "image-to-3d").await;
    assert_eq!(recorded["meshy/image-to-3d/task"], "task-1");

    seen.lock().unwrap().finished = true;
    let again = JobLog::new(recorded);
    let out = again
        .scope(meshy.infer(&request("image-to-3d")))
        .await
        .expect("the mesh comes back");
    assert_eq!(out.parts[0].bytes, b"glTF-bytes");
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.creates.len(),
        1,
        "one submit in all: {:?}",
        seen.creates
    );
    assert!(
        seen.polls.iter().all(|id| id == "task-1"),
        "{:?}",
        seen.polls
    );
}

/// Each phase of a two-phase operation is picked back up where it stood: a
/// restart during the refine polls the preview's refine, and submits nothing.
#[tokio::test]
async fn a_two_phase_operation_resumes_at_its_second_phase() {
    let (base, seen) = mock_meshy().await;
    let meshy = provider(&base);
    let recorded = {
        let log =
            JobLog::new([("meshy/text-to-3d/preview".to_string(), "task-0".to_string())].into());
        // The preview finished before the restart; the refine was submitted.
        seen.lock().unwrap().finished = true;
        let req = request("text-to-3d");
        let made = log.scope(meshy.infer(&req)).await.expect("a mesh");
        assert!(made.content.contains("text-to-3d"));
        log.jobs()
    };
    let seen = seen.lock().unwrap();
    assert_eq!(seen.creates.len(), 1, "only the refine: {:?}", seen.creates);
    assert_eq!(recorded["meshy/text-to-3d/refine"], "task-1");
    assert_eq!(seen.polls.first().map(String::as_str), Some("task-0"));
}

/// A task Meshy no longer has is submitted again, and the answer says so.
#[tokio::test]
async fn a_task_that_is_gone_is_submitted_again_and_the_answer_says_so() {
    let (base, seen) = mock_meshy().await;
    let meshy = provider(&base);
    {
        let mut seen = seen.lock().unwrap();
        seen.finished = true;
        seen.gone.insert("task-old".to_string());
    }
    let log = JobLog::new([("meshy/rig/task".to_string(), "task-old".to_string())].into());
    let out = log
        .scope(meshy.infer(&request("rig")))
        .await
        .expect("a rigged mesh");
    assert!(out.content.contains("task-old"), "{}", out.content);
    assert!(out.content.contains("submitted again"), "{}", out.content);
    assert_eq!(seen.lock().unwrap().creates.len(), 1);
    assert_eq!(log.jobs()["meshy/rig/task"], "task-1");
}

/// A task that failed is not picked back up: the call made again submits a
/// new one.
#[tokio::test]
async fn a_failed_task_is_not_picked_back_up() {
    let (base, _seen) = mock_meshy().await;
    let meshy = provider(&base);
    let log = JobLog::default();
    let mut req = request("image-to-3d");
    req.request_timeout_secs = Some(0);
    let err = log.scope(meshy.infer(&req)).await.unwrap_err();
    assert!(matches!(err, ProviderError::Other(_)), "{err}");
    assert!(log.jobs().is_empty(), "{:?}", log.jobs());
}

/// A task submitted before the restart that failed meanwhile ends the call
/// made again with its failure, and is forgotten: the next call submits a
/// new one rather than polling a task that will never finish.
#[tokio::test]
async fn a_task_that_failed_before_the_call_was_made_again_is_forgotten() {
    let (base, seen) = mock_meshy().await;
    let meshy = provider(&base);
    seen.lock().unwrap().failed.insert("task-old".to_string());
    let log = JobLog::new([("meshy/image-to-3d/task".to_string(), "task-old".to_string())].into());
    let err = log
        .scope(meshy.infer(&request("image-to-3d")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bad mesh"), "{err}");
    assert!(log.jobs().is_empty(), "{:?}", log.jobs());
    assert!(seen.lock().unwrap().creates.is_empty(), "nothing submitted");
}
