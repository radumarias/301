//! QR code rendering as SVG.

use qrcode::{render::svg, EcLevel, QrCode};

fn color(s: Option<&str>, default: &str) -> String {
    match s {
        Some(c)
            if (c.len() == 6 || c.len() == 3 || c.len() == 8)
                && c.chars().all(|ch| ch.is_ascii_hexdigit()) =>
        {
            format!("#{c}")
        }
        _ => default.to_string(),
    }
}

/// Renders `data` as an SVG QR code. Colours are hex without `#`.
pub fn svg(
    data: &str,
    fg: Option<&str>,
    bg: Option<&str>,
    ecc: Option<&str>,
    size: u32,
) -> Result<String, String> {
    let level = match ecc.unwrap_or("M") {
        "L" => EcLevel::L,
        "Q" => EcLevel::Q,
        "H" => EcLevel::H,
        _ => EcLevel::M,
    };
    let code =
        QrCode::with_error_correction_level(data.as_bytes(), level).map_err(|e| e.to_string())?;
    let fg = color(fg, "#000000");
    let bg = color(bg, "#ffffff");
    let size = size.clamp(64, 2048);
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(size, size)
        .quiet_zone(true)
        .dark_color(svg::Color(&fg))
        .light_color(svg::Color(&bg))
        .build())
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders() {
        let s = super::svg(
            "https://301.xorio.rs/abc",
            Some("112233"),
            None,
            Some("H"),
            256,
        )
        .unwrap();
        assert!(s.starts_with("<?xml") || s.starts_with("<svg"));
        assert!(s.contains("#112233"));
        let bad = super::svg("x", Some("javascript:"), None, None, 256).unwrap();
        assert!(!bad.contains("javascript"));
    }
}
