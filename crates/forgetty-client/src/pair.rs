//! Pairing on `forgetty/pair/2` (PROTOCOL_V2 section 7).

use std::time::Duration;

use forgetty_proto::{
    read_msg, write_msg, ClientKind, PairRequest, PairResult, QrPayload, ALPN_PAIR,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey};

use crate::config::{transport_config, ClientOptions};
use crate::error::ClientError;

/// Result of a successful pairing.
#[derive(Debug, Clone)]
pub struct PairedServer {
    /// Address to give to `Client::connect`.
    pub addr: EndpointAddr,
    /// Host name from the QR payload.
    pub machine: String,
    /// Relay URLs from the QR payload, in order. Store them per machine; build
    /// the address again with [`server_addr`].
    pub relays: Vec<String>,
}

/// Build the server address from a node id and relay URLs.
///
/// An app stores `node_id` and the relay URLs of a paired machine
/// ([`PairedServer::relays`]) and calls this on every start.
pub fn server_addr(node_id: &str, relays: &[String]) -> Result<EndpointAddr, ClientError> {
    let id: EndpointId = node_id
        .parse()
        .map_err(|e| ClientError::Invalid(format!("bad node_id in QR payload: {e}")))?;
    let mut addr = EndpointAddr::new(id);
    for url in relays.iter().filter(|r| !r.is_empty()) {
        let relay: RelayUrl = url
            .parse()
            .map_err(|e| ClientError::Invalid(format!("bad relay in QR payload: {e}")))?;
        addr = addr.with_relay_url(relay);
    }
    Ok(addr)
}

/// Build the server address from a QR payload (node id plus all relay URLs).
pub fn qr_to_addr(qr: &QrPayload) -> Result<EndpointAddr, ClientError> {
    server_addr(&qr.node_id, &qr.relay_urls())
}

/// Pair this device with the server in a QR payload.
///
/// `identity` is the key the device will use for `Client::connect`: the
/// server registers its public id.
pub async fn pair(
    qr: &QrPayload,
    identity: SecretKey,
    device_name: &str,
    client_kind: ClientKind,
) -> Result<PairedServer, ClientError> {
    let addr = qr_to_addr(qr)?;
    pair_with_addr(
        addr.clone(),
        &qr.secret,
        identity,
        device_name,
        client_kind,
        &ClientOptions::default(),
    )
    .await?;
    Ok(PairedServer { addr, machine: qr.machine.clone(), relays: qr.relay_urls() })
}

/// Pair with an explicit address and secret (tests, manual entry).
pub async fn pair_with_addr(
    addr: EndpointAddr,
    secret: &str,
    identity: SecretKey,
    device_name: &str,
    client_kind: ClientKind,
    opts: &ClientOptions,
) -> Result<(), ClientError> {
    let endpoint = opts
        .shape(Endpoint::empty_builder())
        .secret_key(identity)
        .relay_mode(opts.relay.mode(&addr))
        .transport_config(transport_config())
        .bind()
        .await
        .map_err(|e| ClientError::Setup(e.to_string()))?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        pair_exchange(&endpoint, addr, secret, device_name, client_kind),
    )
    .await
    .unwrap_or(Err(ClientError::Timeout));
    endpoint.close().await;
    outcome
}

async fn pair_exchange(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    secret: &str,
    device_name: &str,
    client_kind: ClientKind,
) -> Result<(), ClientError> {
    let conn = endpoint
        .connect(addr, ALPN_PAIR)
        .await
        .map_err(|e| ClientError::Failed(format!("cannot reach the server: {e}")))?;
    let (mut send, mut recv) =
        conn.open_bi().await.map_err(|e| ClientError::Failed(e.to_string()))?;
    let request = PairRequest {
        secret: secret.to_string(),
        device_name: device_name.to_string(),
        client_kind,
    };
    write_msg(&mut send, &request).await?;
    let _ = send.finish();
    let result: PairResult = read_msg(&mut recv)
        .await?
        .ok_or_else(|| ClientError::Failed("server closed without a PairResult".into()))?;
    conn.close(0u8.into(), b"done");
    if result.ok {
        Ok(())
    } else {
        Err(ClientError::PairRejected(result.reason.unwrap_or_else(|| "unknown".into())))
    }
}
