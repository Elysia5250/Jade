//! Phase 3 process-level stress tests against the real `jade` binary:
//! 1) 16 isolated headless processes running concurrently against Mock SSE.
//! 2) Shared `--data-dir` race test (fast exit vs slow multi-step process).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

fn jade_bin() -> &'static str {
    env!("CARGO_BIN_EXE_jade")
}

fn short_tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = PathBuf::from("/tmp").join(format!("ehp-{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

async fn read_http_body(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    while let Ok(n) = sock.read(&mut tmp).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let hdr = String::from_utf8_lossy(&buf[..pos]);
            let cl = hdr
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if buf.len() >= pos + 4 + cl {
                break;
            }
        }
    }
    let raw = String::from_utf8_lossy(&buf).to_string();
    raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_16_isolated_headless_processes_concurrent() {
    let dir = short_tmpdir("iso16");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("target.txt"), "TOKEN_OK_42").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reqs = Arc::new(AtomicUsize::new(0));
    let reqs_srv = Arc::clone(&reqs);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let cnt = Arc::clone(&reqs_srv);
            tokio::spawn(async move {
                let body = read_http_body(&mut sock).await;
                cnt.fetch_add(1, Ordering::SeqCst);
                let sse = if body.contains("TOKEN_OK_42") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"found TOKEN_OK_42\"}}],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":10}}\n\ndata: [DONE]\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"r1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"target.txt\\\"}\"}}]}}],\"usage\":{\"prompt_tokens\":40,\"completion_tokens\":12}}\n\ndata: [DONE]\n\n"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let started = Instant::now();
    let mut children = Vec::new();
    for i in 0..16 {
        let data = dir.join(format!("data-{i}"));
        let ws_c = ws.clone();
        let url = format!("http://{addr}/v1");
        children.push(tokio::spawn(async move {
            let out = Command::new(jade_bin())
                .arg("--data-dir")
                .arg(&data)
                .arg("--cwd")
                .arg(&ws_c)
                .arg("--model")
                .arg("m-stress")
                .arg("--dangerously-skip-permissions")
                .arg("--sandbox")
                .arg("off")
                .arg("--output-format")
                .arg("json")
                .arg("-p")
                .arg(format!("proc-{i}: read target.txt"))
                .env("JADE_API_KEY", "sk-stress")
                .env("JADE_BASE_URL", url)
                .env("JADE_PROTOCOL", "openai")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .unwrap();
            (i, out)
        }));
    }

    for h in children {
        let (i, out) = tokio::time::timeout(Duration::from_secs(30), h)
            .await
            .expect("process timed out")
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "proc {i} failed ({}):\nstdout={stdout}\nstderr={stderr}",
            out.status
        );
        let json: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("proc {i} bad json: {e}\n{stdout}"));
        assert_eq!(json["ok"], true);
        assert_eq!(json["tool_calls"], 1);
        assert!(json["result"]
            .as_str()
            .unwrap_or("")
            .contains("TOKEN_OK_42"));
    }

    eprintln!(
        "[stress_headless] 16 concurrent isolated processes passed in {:?}, mock_reqs={}",
        started.elapsed(),
        reqs.load(Ordering::SeqCst)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_shared_data_dir_ephemeral_server_race() {
    let dir = short_tmpdir("shared");
    let data = dir.join("shared-data");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("target.txt"), "SHARED_OK").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let body = read_http_body(&mut sock).await;
                let sse = if body.contains("FAST_EXIT") {
                    // Stay alive long enough for slow sibling to see jade.sock as running, then exit before slow sibling calls tools.
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    "data: {\"choices\":[{\"delta\":{\"content\":\"fast done\"}}]}\n\ndata: [DONE]\n\n"
                } else if body.contains("SHARED_OK") {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"slow verified SHARED_OK\"}}]}\n\ndata: [DONE]\n\n"
                } else {
                    // Slower process issues tool call after fast owner has shut down its EphemeralServer.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"r1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"target.txt\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{sse}",
                    sse.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let mut children = Vec::new();
    for i in 0..4 {
        let data_c = data.clone();
        let ws_c = ws.clone();
        let url = format!("http://{addr}/v1");
        let prompt = if i == 0 {
            "FAST_EXIT".to_string()
        } else {
            format!("SLOW_TASK_{i}: read target.txt")
        };
        children.push(tokio::spawn(async move {
            if i > 0 {
                // Stagger slow processes slightly so process 0 already bound jade.sock.
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let out = Command::new(jade_bin())
                .arg("--data-dir")
                .arg(&data_c)
                .arg("--cwd")
                .arg(&ws_c)
                .arg("--model")
                .arg("m-stress")
                .arg("--dangerously-skip-permissions")
                .arg("--sandbox")
                .arg("off")
                .arg("--output-format")
                .arg("json")
                .arg("-p")
                .arg(prompt)
                .env("JADE_API_KEY", "sk-stress")
                .env("JADE_BASE_URL", url)
                .env("JADE_PROTOCOL", "openai")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .unwrap();
            (i, out)
        }));
    }

    for h in children {
        let (i, out) = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("shared process timed out after 5s")
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "shared proc {i} failed ({}):\nstdout={stdout}\nstderr={stderr}",
            out.status
        );
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        if i >= 1 {
            assert!(
                json["result"].as_str().unwrap_or("").contains("SHARED_OK"),
                "slow shared proc {i} failed tool execution when fast proc exited: {stdout}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}
