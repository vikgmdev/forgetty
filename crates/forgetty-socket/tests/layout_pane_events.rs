//! P-010: `subscribe_layout` forwards pane lifecycle events, and AD-019 pane
//! metadata events (title, cwd, status).

use std::sync::Arc;
use std::time::Duration;

use forgetty_pty::PtySize;
use forgetty_session::SessionManager;
use forgetty_socket::SocketServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

mod common;
use common::{connect, endpoint, Endpoint, Stream};

async fn subscribe(
    sm: Arc<SessionManager>,
) -> (Endpoint, BufReader<Stream>, tokio::task::JoinHandle<()>) {
    let ep = endpoint();
    let path = ep.path.clone();
    let server = SocketServer::new_with_path(path.clone()).unwrap();
    let task = tokio::spawn(async move {
        let _ = server.run_with_streaming(sm, None).await;
    });
    let mut stream = connect(&path).await;
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"subscribe_layout\",\"id\":1}\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut ack = String::new();
    reader.read_line(&mut ack).await.unwrap();
    assert!(ack.contains("\"ok\":true"), "ack was {ack}");
    (ep, reader, task)
}

/// Read notifications until one has `method == want` (and `pane_id` if given).
async fn expect(reader: &mut BufReader<Stream>, want: &str, pane: &str) -> serde_json::Value {
    expect_where(reader, want, pane, |_| true).await
}

/// Like [`expect`], and the notification must also pass `accept`.
async fn expect_where(
    reader: &mut BufReader<Stream>,
    want: &str,
    pane: &str,
    accept: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => {
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                if v["method"] == want && v["params"]["pane_id"] == pane && accept(&v) {
                    return v;
                }
                seen.push(v["method"].as_str().unwrap_or("?").to_string());
            }
            _ => {}
        }
    }
    panic!("no {want} for {pane}; saw {seen:?}");
}

fn size() -> PtySize {
    PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
}

#[tokio::test]
async fn pane_exited_and_pane_closed_are_forwarded() {
    let sm = Arc::new(SessionManager::new());
    let (_dir, mut reader, task) = subscribe(Arc::clone(&sm)).await;
    #[cfg(unix)]
    let cmd = Some(vec!["sh".into(), "-c".into(), "exit 4".into()]);
    #[cfg(windows)]
    let cmd = Some(vec!["cmd.exe".into(), "/c".into(), "exit 4".into()]);
    let (pane, _) = sm.create_tab(0, None, size(), cmd).unwrap();
    let pane = pane.to_string();
    let exited = expect(&mut reader, "pane_exited", &pane).await;
    assert_eq!(exited["params"]["exit_code"], 4);
    expect(&mut reader, "pane_closed", &pane).await;
    task.abort();
}

/// ConPTY itself reports a title set by `OSC 2`, so this one runs on Windows too. The shell may
/// set its own title first, so the test waits for the one it asked for.
#[cfg(windows)]
#[tokio::test]
async fn title_events_are_forwarded() {
    let sm = Arc::new(SessionManager::new());
    let (_ep, mut reader, task) = subscribe(Arc::clone(&sm)).await;
    let script = "[Console]::Out.Write([char]27 + ']2;evt-title' + [char]7); Start-Sleep 30";
    let cmd = Some(vec![
        "powershell.exe".into(),
        "-NoProfile".into(),
        "-Command".into(),
        script.into(),
    ]);
    let (pane, _) = sm.create_tab(0, None, size(), cmd).unwrap();
    let id = pane.to_string();
    expect_where(&mut reader, "title_changed", &id, |v| v["params"]["title"] == "evt-title").await;
    let _ = sm.close_pane(pane);
    task.abort();
}

/// The cwd and status events come from `OSC 7` and `OSC 7501`. They are checked on Unix only:
/// ConPTY does not promise to pass those sequences through.
#[cfg(unix)]
#[tokio::test]
async fn title_cwd_and_status_events_are_forwarded() {
    let sm = Arc::new(SessionManager::new());
    let (_dir, mut reader, task) = subscribe(Arc::clone(&sm)).await;
    let script = "printf '\\033]2;evt-title\\007\\033]7;file:///var/tmp\\007\\033]7501;state=done:msg=RmluaXNoZWQ=\\007'; sleep 30";
    let cmd = Some(vec!["sh".into(), "-c".into(), script.into()]);
    let (pane, _) = sm.create_tab(0, None, size(), cmd).unwrap();
    let id = pane.to_string();
    assert_eq!(expect(&mut reader, "title_changed", &id).await["params"]["title"], "evt-title");
    assert_eq!(expect(&mut reader, "cwd_changed", &id).await["params"]["cwd"], "/var/tmp");
    let st = expect(&mut reader, "status_changed", &id).await;
    assert_eq!(st["params"]["status"], "done");
    assert_eq!(st["params"]["message"], "Finished");
    let _ = sm.close_pane(pane);
    task.abort();
}
