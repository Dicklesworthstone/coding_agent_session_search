//! QR code generation for recovery secrets.
//!
//! Generates QR codes in PNG and SVG formats for easy mobile scanning.
//!
//! Requires the `qr` feature to be enabled for actual QR generation.
//! When disabled, functions return an error explaining the missing feature.

#![allow(unexpected_cfgs)]

use anyhow::{Result, bail};

/// Generate a QR code as PNG bytes
///
/// Returns PNG image data that can be written to a file.
pub fn generate_qr_png(data: &str) -> Result<Vec<u8>> {
    // Use qrcode crate if available
    #[cfg(feature = "qr")]
    {
        use image::Luma;
        use qrcode::QrCode;

        let code = QrCode::new(data.as_bytes())?;
        let image = code.render::<Luma<u8>>().build();

        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageLuma8(image).write_to(
            &mut std::io::Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )?;

        Ok(png_bytes)
    }

    #[cfg(not(feature = "qr"))]
    {
        let _ = data;
        bail!("QR code generation requires the 'qr' feature to be enabled")
    }
}

/// Generate a QR code as SVG string
///
/// Returns SVG markup that can be written to a file.
pub fn generate_qr_svg(data: &str) -> Result<String> {
    #[cfg(feature = "qr")]
    {
        use qrcode::QrCode;
        use qrcode::render::svg;

        let code = QrCode::new(data.as_bytes())?;
        let svg = code
            .render()
            .min_dimensions(200, 200)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build();

        Ok(svg)
    }

    #[cfg(not(feature = "qr"))]
    {
        let _ = data;
        bail!("QR code generation requires the 'qr' feature to be enabled")
    }
}

/// QR code generator (legacy struct interface)
pub struct QrGenerator {
    // Config
}

impl Default for QrGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl QrGenerator {
    pub fn new() -> Self {
        Self {}
    }

    pub fn generate(&self, data: &str, output_path: &std::path::Path) -> Result<()> {
        let png_data = generate_qr_png(data)?;
        std::fs::write(output_path, png_data)?;
        Ok(())
    }
}
