//! `forgetty-pair-test` — minimal iroh dialer for manual QA testing.
//!
//! Connects to a running `forgetty-daemon --allow-pairing` instance and
//! completes the pairing handshake. Used to test AC-6 and AC-14 without
//! an Android device.
//!
//! # Usage
//!
//! ```text
//! # Start daemon with pairing enabled:
//! forgetty-daemon --foreground --allow-pairing &
//!
//! # Get node_id from daemon output or:
//! forgetty-daemon --show-pairing-qr
//!
//! # Dial and pair:
//! forgetty-pair-test --dial <node_id>
//! ```

use clap::Parser;
use iroh::{Endpoint, EndpointAddr, SecretKey, endpoint::presets};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const FORGETTY_PAIRING_ALPN: &[u8] = b"forgetty/pair/1";

#[derive(Parser, Debug)]
#[command(name = "forgetty-pair-test")]
#[command(about = "Minimal iroh pairing test client for QA without an Android device")]
struct Args {
    /// The node ID (iroh EndpointId) of the daemon to pair with.
    ///
    /// Obtain by running: `forgetty-daemon --show-pairing-qr`
    #[arg(long)]
    dial: String,
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(run()) {
        eprintln!("pair-test error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    // Parse the node_id string into an EndpointId (PublicKey), then wrap in
    // EndpointAddr. iroh 0.97: PublicKey implements FromStr (base32).
    let endpoint_id: iroh::EndpointId = args
        .dial
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid node_id '{}': {e}", args.dial))?;
    let addr: EndpointAddr = EndpointAddr::from(endpoint_id);

    // Create a fresh ephemeral endpoint for this test run.
    let secret_key = SecretKey::generate(&mut rand::rng());
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind client endpoint: {e}"))?;

    eprintln!("pair-test: dialing {}", args.dial);

    // Connect to the daemon.
    let conn = ep
        .connect(addr, FORGETTY_PAIRING_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;

    eprintln!("pair-test: connected, remote_id={}", conn.remote_id());

    // The daemon opens a bi-directional stream and sends a greeting line.
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| anyhow::anyhow!("accept_bi failed: {e}"))?;

    // Read the daemon's greeting: {"v":1,"status":"ok","machine":"<hostname>"}
    let mut lines = BufReader::new(&mut recv).lines();
    let greeting_line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("daemon closed stream without greeting"))?;
    eprintln!("pair-test: greeting: {greeting_line}");

    // Send back our device name.
    let response = serde_json::json!({ "v": 1, "name": "pair-test-device" });
    let mut response_line = serde_json::to_string(&response)?;
    response_line.push('\n');
    send.write_all(response_line.as_bytes()).await?;
    send.flush().await?;

    // Wait for stream to close (daemon closes after pairing).
    drop(lines);
    // Give daemon time to process and close.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    println!("paired ok");

    conn.close(0u8.into(), b"done");
    ep.close().await;
    Ok(())
}
