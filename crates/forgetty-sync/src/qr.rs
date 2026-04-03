//! QR code generation for the pairing payload.
//!
//! Two output modes:
//! - `qr_to_ascii`: Dense Unicode art for terminal output (`--show-pairing-qr`).
//! - `qr_to_png`: PNG bytes for GTK `Image` widget (via base64 in the socket RPC).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The default iroh EU relay used in the QR payload.
///
/// Hardcoded for T-052 per spec. Custom relay configuration is out of scope.
pub const DEFAULT_RELAY: &str = "https://euw1-1.relay.iroh.network/";

/// QR payload as encoded in the QR code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrPayload {
    /// Protocol version. Always `1` in T-052.
    pub v: u32,
    /// The daemon's iroh `EndpointId` as a base32 string (spec: "node_id").
    pub node_id: String,
    /// The machine hostname.
    pub machine: String,
    /// The iroh relay URL.
    pub relay: String,
}

impl QrPayload {
    /// Build a `QrPayload` from a node ID string and the current hostname.
    pub fn new(node_id_str: String) -> Self {
        let machine = hostname::get()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string());
        QrPayload { v: 1, node_id: node_id_str, machine, relay: DEFAULT_RELAY.to_string() }
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
