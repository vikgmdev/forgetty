//! `forgetty-pair-test`: minimal iroh pairing client for QA without a phone.
//!
//! Pairs this machine's QA identity with a running server on ALPN
//! `forgetty/pair/2` (AD-021). The secret comes from the `payload:` line that
//! `forgetty pair` prints.
//!
//! ```text
//! forgetty pair                       # prints: payload: {"v":2,"node_id":...,"secret":...}
//! forgetty-pair-test --payload '<that json>'
//! forgetty-pair-test --dial <node_id> --secret <secret>
//! ```
//!
//! Exit 0 when paired, 1 when the server refused (the reason is printed).

use std::path::PathBuf;

use clap::Parser;
use iroh::{endpoint::presets, Endpoint, EndpointAddr, SecretKey};

#[derive(Parser, Debug)]
#[command(name = "forgetty-pair-test")]
#[command(about = "Minimal iroh pairing test client for QA without an Android device")]
struct Args {
    /// Payload JSON from `forgetty pair` (gives node id and secret).
    #[arg(long)]
    payload: Option<String>,

    /// Node id of the server. Needed unless `--payload` is given.
    #[arg(long)]
    dial: Option<String>,

    /// Pairing secret. Needed unless `--payload` is given.
    #[arg(long)]
    secret: Option<String>,

    /// Device name to register.
    #[arg(long, default_value = "pair-test-device")]
    name: String,

    /// Identity key file (default `<data dir>/forgetty/pair-test.key`).
    #[arg(long)]
    identity: Option<PathBuf>,
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    match runtime.block_on(run()) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("pair-test error: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> anyhow::Result<bool> {
    let args = Args::parse();

    let (node_id, secret) = match &args.payload {
        Some(json) => {
            let v: serde_json::Value = serde_json::from_str(json)?;
            let node =
                v["node_id"].as_str().ok_or_else(|| anyhow::anyhow!("payload has no node_id"))?;
            let secret =
                v["secret"].as_str().ok_or_else(|| anyhow::anyhow!("payload has no secret"))?;
            (node.to_string(), secret.to_string())
        }
        None => (
            args.dial.clone().ok_or_else(|| anyhow::anyhow!("--dial or --payload is required"))?,
            args.secret
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--secret or --payload is required"))?,
        ),
    };

    let endpoint_id: iroh::EndpointId =
        node_id.parse().map_err(|e| anyhow::anyhow!("invalid node_id '{node_id}': {e}"))?;

    let secret_key = load_or_generate_key(args.identity)?;
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind client endpoint: {e}"))?;

    eprintln!("pair-test: dialing {node_id} on forgetty/pair/2");
    let result = forgetty_sync::pairing::request_pairing(
        &ep,
        EndpointAddr::from(endpoint_id),
        &secret,
        &args.name,
        "cli",
    )
    .await;
    ep.close().await;

    let result = result?;
    if result.ok {
        println!("paired ok");
        Ok(true)
    } else {
        println!("pair refused: {}", result.reason.as_deref().unwrap_or("unknown"));
        Ok(false)
    }
}

/// Load the QA identity, or generate and persist a new one.
fn load_or_generate_key(path: Option<PathBuf>) -> anyhow::Result<SecretKey> {
    let path = path.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("forgetty")
            .join("pair-test.key")
    });

    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("{} is not 32 bytes; delete it to regenerate", path.display())
        })?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        std::fs::create_dir_all(path.parent().expect("identity path has parent"))?;
        std::fs::write(&path, key.to_bytes())?;
        forgetty_core::fsutil::acl::restrict_to_user(&path)?;
        eprintln!("pair-test: new identity generated, saved to {}", path.display());
        Ok(key)
    }
}
