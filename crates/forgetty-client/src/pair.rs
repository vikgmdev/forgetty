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
    /// The server uses no relay (a WireGuard-only server). Store it with the
    /// machine and set `ClientOptions::relays_disabled` from it.
    pub relays_disabled: bool,
    /// The direct addresses from the QR payload. They are pinned: store them
    /// with the machine and give them back as `ClientOptions::pinned_addrs`
    /// (or use [`PairedServer::options`]).
    pub pinned_addrs: Vec<std::net::SocketAddr>,
}

impl PairedServer {
    /// Client options for this machine: `relays_disabled` and `pinned_addrs`
    /// as paired.
    pub fn options(&self) -> ClientOptions {
        ClientOptions {
            relays_disabled: self.relays_disabled,
            pinned_addrs: self.pinned_addrs.clone(),
            ..Default::default()
        }
    }
}

/// Build the server address from a node id and relay URLs.
///
/// An app stores `node_id` and the relay URLs of a paired machine
/// ([`PairedServer::relays`]) and calls this on every start.
pub fn server_addr(node_id: &str, relays: &[String]) -> Result<EndpointAddr, ClientError> {
    server_addr_direct(node_id, relays, &[])
}

/// [`server_addr`] with direct addresses too: the ones from the pairing
/// payload. They are pinned: the client dials them on every network and never
/// forgets them.
pub fn server_addr_direct(
    node_id: &str,
    relays: &[String],
    direct: &[std::net::SocketAddr],
) -> Result<EndpointAddr, ClientError> {
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
    for d in forgetty_proto::usable_pinned_addrs(direct.iter().copied()) {
        addr = addr.with_ip_addr(d);
    }
    Ok(addr)
}

/// The same address with only its usable direct addresses (see
/// [`forgetty_proto::usable_pinned_addrs`]): relays and the id are kept. Every
/// path from an app or a payload into a server address goes through it.
pub(crate) fn clean_direct(addr: EndpointAddr) -> EndpointAddr {
    let ips: Vec<std::net::SocketAddr> = addr.ip_addrs().copied().collect();
    let usable = forgetty_proto::usable_pinned_addrs(ips.iter().copied());
    if usable == ips {
        return addr;
    }
    let others = addr.addrs.iter().filter(|a| !matches!(a, iroh::TransportAddr::Ip(_))).cloned();
    EndpointAddr::from_parts(addr.id, others)
        .with_addrs(usable.into_iter().map(iroh::TransportAddr::Ip))
}

/// Build the server address from a QR payload (node id plus all relay URLs).
pub fn qr_to_addr(qr: &QrPayload) -> Result<EndpointAddr, ClientError> {
    // A server without relays has none to dial, whatever `relay` says.
    let relays = if qr.relays_disabled { Vec::new() } else { qr.relay_urls() };
    server_addr_direct(&qr.node_id, &relays, &qr.direct_addrs())
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
    let direct = qr.direct_addrs();
    pair_with_addr(
        addr.clone(),
        &qr.secret,
        identity,
        device_name,
        client_kind,
        &ClientOptions {
            relays_disabled: qr.relays_disabled,
            pinned_addrs: direct.clone(),
            ..Default::default()
        },
    )
    .await?;
    Ok(PairedServer {
        addr,
        machine: qr.machine.clone(),
        relays: if qr.relays_disabled { Vec::new() } else { qr.relay_urls() },
        relays_disabled: qr.relays_disabled,
        pinned_addrs: direct,
    })
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
    let addr = clean_direct(addr);
    let endpoint = opts
        .shape(Endpoint::empty_builder())
        .secret_key(identity)
        .relay_mode(opts.relay_mode(&addr))
        .portmapper_config(opts.portmapper_config(&addr))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> EndpointId {
        SecretKey::generate(&mut rand::rng()).public()
    }

    #[test]
    fn server_addr_direct_drops_unusable_pins() {
        let node = id().to_string();
        let direct: Vec<std::net::SocketAddr> = [
            "0.0.0.0:47297",
            "[::ffff:224.0.0.1]:47297",
            "169.254.3.3:47297",
            "10.230.1.14:0",
            "10.230.1.14:47297",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        let addr = server_addr_direct(&node, &[], &direct).unwrap();
        let got: Vec<_> = addr.ip_addrs().copied().collect();
        assert_eq!(got, vec!["10.230.1.14:47297".parse().unwrap()]);
    }

    #[test]
    fn clean_direct_keeps_relays_and_good_addresses() {
        let relay: RelayUrl = "https://relay.example.com/".parse().unwrap();
        let addr = EndpointAddr::new(id())
            .with_relay_url(relay.clone())
            .with_ip_addr("0.0.0.0:1".parse().unwrap())
            .with_ip_addr("[::ffff:255.255.255.255]:1".parse().unwrap())
            .with_ip_addr("127.0.0.1:47297".parse().unwrap());
        let cleaned = clean_direct(addr.clone());
        assert_eq!(cleaned.id, addr.id);
        assert_eq!(cleaned.relay_urls().cloned().collect::<Vec<_>>(), vec![relay]);
        assert_eq!(
            cleaned.ip_addrs().copied().collect::<Vec<_>>(),
            vec!["127.0.0.1:47297".parse().unwrap()]
        );
    }
}
