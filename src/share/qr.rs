//! QR-code rendering for a VLESS share URI.
//!
//! Returns a PNG byte buffer at `Content-Type: image/png`. We render with a
//! 4-pixel quiet zone so the image is decodable by every phone scanner we
//! care about (iOS / Android / 第三方 clients), and the dark / light colours
//! are pure black-on-white for maximum contrast.

use image::{ExtendedColorType, ImageEncoder};
use qrcode::QrCode;

use crate::error::{AppError, AppResult};

/// Encode `text` as a PNG QR code. The output is a standalone PNG byte
/// stream suitable for streaming straight to a client.
pub fn render_png(text: &str) -> AppResult<Vec<u8>> {
    let code = QrCode::new(text.as_bytes()).map_err(|e| AppError::Xray(format!("qrcode: {e}")))?;
    let image = code
        .render::<image::Luma<u8>>()
        .min_dimensions(200, 200)
        .build();

    let mut out = Vec::with_capacity(image.len() * 4);
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::L8,
        )
        .map_err(|e| AppError::Xray(format!("png encode: {e}")))?;
    Ok(out)
}

/// The first eight bytes of a PNG: `\x89PNG\r\n\x1a\n`. Exposed so the
/// `image/png` endpoint can be smoke-tested by magic-byte match.
pub const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_a_valid_png() {
        let bytes = render_png("vless://example").unwrap();
        assert!(bytes.len() > 16, "png should not be tiny");
        assert_eq!(&bytes[..8], &PNG_MAGIC);
        // IHDR chunk starts at offset 8 and reports width/height as
        // big-endian u32 — for a 200×200 image the dimensions are 200, 200.
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        assert!(w >= 200 && h >= 200);
    }
}
