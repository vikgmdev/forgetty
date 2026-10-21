//! `forgetty-stream-test` — terminal stream QA test client.
//!
//! Connects to a running `forgetty-server` instance via iroh QUIC using the
//! `forgetty/stream/1` ALPN. Sends a `Subscribe` message for a given pane,
//! reads the `FullSnapshot` response, and then reads live `PtyBytes` frames.
//!
//! Used for QA of T-053 without an Android device.
//!
//! # Typical workflow
//!
//! ```text
//! # Terminal 1: start daemon (with a pane already open or --allow-pairing for first pair)
//! forgetty-server --foreground --allow-pairing &
//!
//! # Get node_id
//! forgetty-server --show-pairing-qr
//!
//! # Terminal 2: pair first (uses the pair-test.key identity)
//! forgetty-pair-test --dial <node_id>
//!
//! # Terminal 3: run stream test (using same pair-test.key identity, so it's authorized)
//! forgetty-stream-test --dial <node_id> --pane-id <uuid>
//!
//! # Or: list panes via socket then stream
//! forgetty-stream-test --dial <node_id> --list-panes
//! ```
//!
//! # Backpressure test
//!
//! ```text
//! # In daemon pane, run:
//! cat /dev/urandom | base64
//!
//! # Then connect with --stress flag to observe that no disconnect happens
//! forgetty-stream-test --dial <node_id> --pane-id <uuid> --stress
//! ```

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use iroh::{endpoint::presets, Endpoint, EndpointAddr, SecretKey};
use tokio::io::AsyncWriteExt;

use forgetty_server::iroh_terminal::{ClientMsg, DaemonMsg, FORGETTY_STREAM_ALPN};

/// Maximum frame size accepted from the daemon (must match the
/// `MAX_FRAME_SIZE` constant in `forgetty_server::iroh_terminal`).
const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "forgetty-stream-test")]
#[command(about = "Terminal stream QA test client for T-053 (no Android required)")]
struct Args {
    /// The iroh node ID (EndpointId) of the daemon to connect to.
    ///
    /// Obtain by running: `forgetty-server --show-pairing-qr`
    #[arg(long)]
    dial: String,

    /// Pane UUID to subscribe to.
    ///
    /// Obtain from `forgetty-server` logs or via socket `list_tabs`.
    #[arg(long)]
    pane_id: Option<String>,

    /// Just list panes via the daemon socket, then exit.
    #[arg(long)]
    list_panes: bool,

    /// Print raw PtyBytes data as UTF-8 text (lossy) to stdout.
    #[arg(long)]
    print_output: bool,

    /// Run until Ctrl-C (stress test for backpressure / no-disconnect guarantee).
    ///
    /// Reads PtyBytes indefinitely. Does not print them to avoid output I/O
    /// becoming the bottleneck. Reports total bytes received every 5 seconds.
    #[arg(long)]
    stress: bool,

    /// First pair this identity with the server (like `forgetty-pair-test`),
    /// then connect for streaming. Needs `--secret` or `--payload`.
    #[arg(long)]
    pair_first: bool,

    /// Pairing secret for `--pair-first`.
    #[arg(long)]
    secret: Option<String>,

    /// Payload JSON from `forgetty pair`, an alternative to `--secret`.
    #[arg(long)]
    payload: Option<String>,

    /// Identity key file (default `<data dir>/forgetty/pair-test.key`).
    #[arg(long)]
    identity: Option<PathBuf>,

    /// Stay attached for this many seconds after subscribing, then exit 0.
    /// Used to simulate a client that attaches and later leaves.
    #[arg(long)]
    hold_secs: Option<u64>,

    /// In non-stress mode, exit after this many `PtyBytes` messages have been
    /// observed. Default 5 preserves the historical "PASS: received N PtyBytes
    /// messages" smoke-test behavior. The typometer (`scripts/perf/typometer.sh`)
    /// passes `--max-msgs 1` because its `--prepare` hook only causes one byte
    /// of PTY local-echo per hyperfine iteration; waiting for 5 would hang.
    #[arg(long, default_value_t = 5)]
    max_msgs: usize,

    /// After subscribing, send this text plus a newline to the pane as
    /// keyboard input (`SendInput`). Used by `scripts/p0_gate.sh`.
    #[arg(long)]
    send_line: Option<String>,

    /// Wait until this substring shows up in the pane output (snapshot lines
    /// or `PtyBytes`). Prints `PASS: expected output found` and exits 0 when
    /// seen, exits 1 on `--timeout-secs`. Overrides `--max-msgs`.
    #[arg(long)]
    expect: Option<String>,

    /// Give up after this many seconds when `--expect` is set.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(run()) {
        eprintln!("stream-test error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    // Parse node_id.
    let endpoint_id: iroh::EndpointId =
        args.dial.parse().map_err(|e| anyhow::anyhow!("invalid node_id '{}': {e}", args.dial))?;

    // Load or generate identity. We reuse the pair-test.key so the same device_id
    // is recognized without needing a separate pairing step when already paired.
    let secret_key = load_or_generate_key(args.identity.clone())?;

    // Bind the client endpoint (no ALPNs needed on dialing side).
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind client endpoint: {e}"))?;

    // Optionally pair first.
    if args.pair_first {
        eprintln!("stream-test: pairing first with --pair-first ...");
        let secret = match (&args.payload, &args.secret) {
            (Some(json), _) => serde_json::from_str::<serde_json::Value>(json)?["secret"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("payload has no secret"))?,
            (None, Some(s)) => s.clone(),
            (None, None) => anyhow::bail!("--pair-first needs --secret or --payload"),
        };
        pair_with_daemon(&ep, endpoint_id, &secret).await?;
        eprintln!("stream-test: paired ok, now connecting for streaming");
    }

    // Connect with the stream ALPN (shared by --list-panes and --pane-id).
    let addr = EndpointAddr::from(endpoint_id);
    eprintln!("stream-test: connecting to {} with ALPN forgetty/stream/1 ...", args.dial);
    let conn = ep
        .connect(addr, FORGETTY_STREAM_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
    eprintln!("stream-test: connected, remote_id={}", conn.remote_id());

    // Open a bidirectional stream (Android side opens the stream).
    let (mut send, mut recv) =
        conn.open_bi().await.map_err(|e| anyhow::anyhow!("open_bi failed: {e}"))?;

    // Dispatch on --list-panes vs --pane-id.
    if args.list_panes {
        let payload = rmp_serde::to_vec_named(&ClientMsg::ListPanes)?;
        let len = (payload.len() as u32).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        send.flush().await?;
        eprintln!("stream-test: sent ListPanes");
    } else {
        let pid = args
            .pane_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--pane-id <uuid> is required (or use --list-panes)"))?;
        let payload = rmp_serde::to_vec_named(&ClientMsg::Subscribe { pane_id: pid.clone() })?;
        let len = (payload.len() as u32).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        send.flush().await?;
        eprintln!("stream-test: sent Subscribe {{ pane_id: {} }}", pid);

        if let Some(line) = &args.send_line {
            let mut data = line.clone().into_bytes();
            data.push(b'\n');
            let payload = rmp_serde::to_vec_named(&ClientMsg::SendInput { pane_id: pid, data })?;
            send.write_all(&(payload.len() as u32).to_be_bytes()).await?;
            send.write_all(&payload).await?;
            send.flush().await?;
            eprintln!("stream-test: sent SendInput {line:?}");
        }
    }

    // Read messages from daemon.
    let mut total_pty_bytes: usize = 0;
    let mut msg_count: usize = 0;
    let start = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();

    let deadline = start + Duration::from_secs(args.timeout_secs);
    let mut seen = String::new();
    let mut expect_found = false;

    loop {
        // Read frame length.
        let msg = if let Some(hold) = args.hold_secs {
            let left = (start + Duration::from_secs(hold))
                .saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(left, read_frame(&mut recv)).await {
                Ok(m) => m?,
                Err(_) => {
                    println!("PASS: held attachment for {hold}s");
                    break;
                }
            }
        } else if args.expect.is_some() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(left, read_frame(&mut recv)).await {
                Ok(m) => m?,
                Err(_) => {
                    println!("FAIL: expected output not seen within {}s", args.timeout_secs);
                    anyhow::bail!("timeout waiting for {:?}", args.expect);
                }
            }
        } else {
            read_frame(&mut recv).await?
        };

        match &msg {
            DaemonMsg::FullSnapshot { rows, cols, lines, cursor_row, cursor_col, .. } => {
                eprintln!(
                    "stream-test: FullSnapshot: {}x{} rows, cursor=({},{}), {} lines",
                    rows,
                    cols,
                    cursor_row,
                    cursor_col,
                    lines.len()
                );
                // Print the first few lines for visual inspection.
                for (i, line) in lines.iter().take(5).enumerate() {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        eprintln!("  row {:2}: {:?}", i, trimmed);
                    }
                }
                for line in lines {
                    seen.push_str(line);
                    seen.push('\n');
                }
                if !args.stress && args.expect.is_none() && args.hold_secs.is_none() {
                    // In non-stress mode, one snapshot is enough for validation.
                    println!("PASS: FullSnapshot received ({}x{})", rows, cols);
                }
            }
            DaemonMsg::PtyBytes { data, pane_id: _ } => {
                total_pty_bytes += data.len();
                msg_count += 1;

                if args.print_output {
                    print!("{}", String::from_utf8_lossy(data));
                }
                if args.expect.is_some() {
                    seen.push_str(&String::from_utf8_lossy(data));
                }

                if args.stress {
                    // Print throughput every 5 seconds.
                    if last_report.elapsed() >= Duration::from_secs(5) {
                        let secs = start.elapsed().as_secs_f64();
                        let kbps = (total_pty_bytes as f64 / 1024.0) / secs;
                        eprintln!(
                            "stream-test: stress: {} msgs, {} bytes total ({:.1} KB/s)",
                            msg_count, total_pty_bytes, kbps
                        );
                        last_report = std::time::Instant::now();
                    }
                } else if msg_count == 1 && args.expect.is_none() && args.hold_secs.is_none() {
                    println!("PASS: first PtyBytes received ({} bytes)", data.len());
                }

                // In non-stress mode, exit after receiving the configured
                // number of PtyBytes messages (default 5; typometer passes 1).
                if !args.stress
                    && args.expect.is_none()
                    && args.hold_secs.is_none()
                    && msg_count >= args.max_msgs
                {
                    println!("PASS: received {} PtyBytes messages", msg_count);
                    break;
                }
            }
            DaemonMsg::ScrollbackPage { from_row, lines, .. } => {
                eprintln!(
                    "stream-test: ScrollbackPage: from_row={}, {} lines",
                    from_row,
                    lines.len()
                );
                println!("PASS: ScrollbackPage received ({} lines)", lines.len());
            }
            DaemonMsg::PaneGone { pane_id } => {
                eprintln!("stream-test: PaneGone for pane {}", pane_id);
                println!("PASS: PaneGone received");
                break;
            }
            DaemonMsg::PaneList { panes } => {
                eprintln!("stream-test: PaneList: {} panes", panes.len());
                for (i, p) in panes.iter().enumerate() {
                    eprintln!(
                        "  pane {:2}: id={} title={:?} cwd={:?} active={}",
                        i, p.id, p.title, p.cwd, p.is_active
                    );
                }
                println!("PASS: PaneList received ({} panes)", panes.len());
                break;
            }
            DaemonMsg::Error { message } => {
                eprintln!("stream-test: daemon error: {}", message);
                println!("FAIL: daemon returned Error: {}", message);
                break;
            }
        }

        if let Some(want) = &args.expect {
            if seen.contains(want.as_str()) {
                println!("PASS: expected output found ({want:?})");
                expect_found = true;
                break;
            }
        }
    }

    // Send Unsubscribe before closing.
    let unsub = ClientMsg::Unsubscribe;
    let payload = rmp_serde::to_vec_named(&unsub)?;
    let len = (payload.len() as u32).to_be_bytes();
    let _ = send.write_all(&len).await;
    let _ = send.write_all(&payload).await;
    let _ = send.flush().await;

    conn.close(0u8.into(), b"done");
    ep.close().await;

    if args.expect.is_some() && !expect_found {
        anyhow::bail!("stream ended before the expected output appeared");
    }

    if args.stress {
        let secs = start.elapsed().as_secs_f64();
        let kbps = (total_pty_bytes as f64 / 1024.0) / secs;
        println!(
            "PASS: stress test completed — {} msgs, {} bytes, {:.1} KB/s",
            msg_count, total_pty_bytes, kbps
        );
    }

    Ok(())
}

/// Read one length-prefixed MessagePack frame and decode it as `DaemonMsg`.
async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> anyhow::Result<DaemonMsg> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| anyhow::anyhow!("read length prefix failed: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len == 0 || len > MAX_FRAME_SIZE {
        anyhow::bail!("frame length {len} out of range");
    }

    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.map_err(|e| anyhow::anyhow!("read payload failed: {e}"))?;

    rmp_serde::from_slice::<DaemonMsg>(&payload)
        .map_err(|e| anyhow::anyhow!("failed to deserialize DaemonMsg: {e}"))
}

/// Pair with the server on `forgetty/pair/2` (AD-021).
async fn pair_with_daemon(
    ep: &Endpoint,
    endpoint_id: iroh::EndpointId,
    secret: &str,
) -> anyhow::Result<()> {
    let result = forgetty_sync::pairing::request_pairing(
        ep,
        EndpointAddr::from(endpoint_id),
        secret,
        "stream-test-device",
        "cli",
    )
    .await?;
    if result.ok {
        Ok(())
    } else {
        println!("pair refused: {}", result.reason.as_deref().unwrap_or("unknown"));
        anyhow::bail!("pairing refused: {}", result.reason.as_deref().unwrap_or("unknown"))
    }
}

/// Load or generate the stream-test key from
/// `~/.local/share/forgetty/pair-test.key`.
///
/// Reuses `pair-test.key` so that a device paired via `forgetty-pair-test`
/// is also recognized by `forgetty-stream-test` without a separate pairing step.
fn load_or_generate_key(path: Option<PathBuf>) -> anyhow::Result<SecretKey> {
    let path: PathBuf = path.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("forgetty")
            .join("pair-test.key")
    });

    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("pair-test.key is not 32 bytes; delete it to regenerate")
        })?;
        eprintln!("stream-test: loaded identity from {}", path.display());
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        std::fs::create_dir_all(path.parent().expect("path has parent"))?;
        std::fs::write(&path, key.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        eprintln!("stream-test: new identity generated, saved to {}", path.display());
        Ok(key)
    }
}
