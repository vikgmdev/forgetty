//! QR code generation for the pairing payload.
//!
//! Two output modes:
//! - `qr_to_ascii`: Dense Unicode art for terminal output (`--show-pairing-qr`).
//! - `qr_to_png`: PNG bytes for GTK `Image` widget (via base64 in the socket RPC).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The default iroh EU relay used in the QR payload when the server has no
/// relay of its own (`relay_urls` / `--relay-url`).
pub const DEFAULT_RELAY: &str = "https://euw1-1.relay.iroh.network/";

/// QR payload as encoded in the QR code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrPayload {
    /// Payload version. `2` since AD-021 (adds `secret`).
    pub v: u32,
    /// The daemon's iroh `EndpointId` as a base32 string (spec: "node_id").
    pub node_id: String,
    /// The machine hostname.
    pub machine: String,
    /// The iroh relay URL. With several relays this is the first one.
    pub relay: String,
    /// All relay URLs of the server including `relay`. Left out of the JSON
    /// when empty, so a server without own relays writes the v2 payload as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    /// The server uses no relay and publishes nothing to n0. `relay` is then
    /// empty. Left out of the JSON when `false`, so an older client reads the
    /// payload as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relays_disabled: bool,
    /// Direct addresses (`ip:port`) the server is bound to, when it was told
    /// which ones to bind. Left out of the JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct: Vec<String>,
    /// One-time pairing secret, 16 random bytes in base32 (AD-021).
    pub secret: String,
}

impl QrPayload {
    /// Build a `QrPayload` from a node ID string, a pairing secret and the
    /// current hostname.
    pub fn new(node_id_str: String, secret: String) -> Self {
        let machine = hostname::get()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string());
        QrPayload {
            v: 2,
            node_id: node_id_str,
            machine,
            relay: DEFAULT_RELAY.to_string(),
            relays: Vec::new(),
            relays_disabled: false,
            direct: Vec::new(),
            secret,
        }
    }

    /// Like [`new`](Self::new), with the relays the server was configured with
    /// (`--relay-url`, `relay_urls`). An empty list keeps the default relay.
    pub fn with_relays(node_id_str: String, secret: String, relays: &[String]) -> Self {
        let mut p = Self::new(node_id_str, secret);
        if let Some(first) = relays.first() {
            p.relay = first.clone();
            if relays.len() > 1 {
                p.relays = relays.to_vec();
            }
        }
        p
    }

    /// A payload for a server that uses no relay (WireGuard-only): no relay
    /// URL, `relays_disabled` set, and the direct addresses it is bound to.
    pub fn without_relays(
        node_id_str: String,
        secret: String,
        direct: &[std::net::SocketAddr],
    ) -> Self {
        let mut p = Self::new(node_id_str, secret);
        p.relay = String::new();
        p.relays_disabled = true;
        p.direct = direct.iter().map(ToString::to_string).collect();
        p
    }

    /// Serialise the payload to a compact JSON string (used as QR data).
    pub fn to_json(&self) -> Result<String, QrError> {
        serde_json::to_string(self).map_err(QrError::Json)
    }
}

/// Errors from QR code generation.
#[derive(Debug, Error)]
pub enum QrError {
    #[error("JSON serialisation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("QR encode error: {0}")]
    Encode(String),
    #[error("PNG encode error: {0}")]
    Png(String),
}

/// Generate an ASCII/Unicode QR code string for terminal output.
///
/// Uses the `Dense1x2` Unicode renderer which packs two rows per terminal line,
/// giving a compact square output.
pub fn qr_to_ascii(payload: &QrPayload) -> Result<String, QrError> {
    use qrcode::{render::unicode, QrCode};
    let json = payload.to_json()?;
    let code = QrCode::new(json.as_bytes()).map_err(|e| QrError::Encode(e.to_string()))?;
    let rendered = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .quiet_zone(true)
        .build();
    Ok(rendered)
}

/// Generate a PNG QR code as raw bytes for display in the GTK settings panel.
///
/// `module_size`: pixels per QR module. Use `8` for a ~300px image at
/// QR version 3 (37 modules). GTK will scale via `Image::set_pixel_size(250)`.
pub fn qr_to_png(payload: &QrPayload, module_size: u32) -> Result<Vec<u8>, QrError> {
    use image::{codecs::png::PngEncoder, DynamicImage, ImageEncoder, Luma};
    use qrcode::QrCode;

    let json = payload.to_json()?;
    let code = QrCode::new(json.as_bytes()).map_err(|e| QrError::Encode(e.to_string()))?;

    // Render to a grayscale image buffer using qrcode defaults for dark/light.
    // dark = 0x00 (black), light = 0xFF (white).
    let img: image::ImageBuffer<Luma<u8>, Vec<u8>> = code
        .render::<Luma<u8>>()
        .module_dimensions(module_size, module_size)
        .quiet_zone(true)
        .dark_color(Luma([0u8]))
        .light_color(Luma([255u8]))
        .build();

    // Encode to PNG bytes.
    let dynamic = DynamicImage::ImageLuma8(img);
    let mut png_bytes: Vec<u8> = Vec::new();
    let encoder = PngEncoder::new(&mut png_bytes);
    let rgba = dynamic.to_rgba8();
    encoder
        .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ColorType::Rgba8.into())
        .map_err(|e| QrError::Png(e.to_string()))?;
    Ok(png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_without_relays_carries_the_flag_and_the_direct_addresses() {
        let direct: Vec<std::net::SocketAddr> = vec!["10.230.1.14:47297".parse().unwrap()];
        let p = QrPayload::without_relays("n".into(), "S".into(), &direct);
        assert!(p.relay.is_empty() && p.relays_disabled);
        let json = p.to_json().unwrap();
        assert!(json.contains("\"relays_disabled\":true"), "{json}");
        assert!(json.contains("10.230.1.14:47297"), "{json}");
    }

    #[test]
    fn a_payload_with_relays_writes_neither_new_field() {
        let json = QrPayload::with_relays("n".into(), "S".into(), &[]).to_json().unwrap();
        assert!(!json.contains("relays_disabled") && !json.contains("direct"), "{json}");
    }

    #[test]
    fn default_payload_is_unchanged() {
        let p = QrPayload::with_relays("n".into(), "S".into(), &[]);
        assert_eq!(p.relay, DEFAULT_RELAY);
        assert!(!p.to_json().unwrap().contains("relays"));
    }

    #[test]
    fn one_relay_replaces_the_default() {
        let p = QrPayload::with_relays("n".into(), "S".into(), &["https://r.example.org/".into()]);
        assert_eq!(p.relay, "https://r.example.org/");
        assert!(!p.to_json().unwrap().contains("relays"));
    }

    #[test]
    fn several_relays_are_listed_in_order() {
        let urls = vec!["https://a.example.org/".to_string(), "https://b.example.org/".to_string()];
        let p = QrPayload::with_relays("n".into(), "S".into(), &urls);
        assert_eq!(p.relay, urls[0]);
        assert_eq!(p.relays, urls);
        assert!(p
            .to_json()
            .unwrap()
            .contains(r#""relays":["https://a.example.org/","https://b.example.org/"]"#));
    }
}
