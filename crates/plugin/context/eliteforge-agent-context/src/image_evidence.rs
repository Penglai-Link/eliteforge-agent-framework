//! Bounded local preparation of host-authorized image evidence. No storage/network IO.
use base64::Engine;
use eliteforge_agent_core::{AgentError, MediaContent, MediaSource};
use image::{ImageFormat, ImageReader};
use std::io::Cursor;
#[derive(Clone, Copy)]
pub struct ImageEvidenceOptions {
    pub max_side: u32,
    pub max_encoded_bytes: usize,
}
impl Default for ImageEvidenceOptions {
    fn default() -> Self {
        Self {
            max_side: 768,
            max_encoded_bytes: 8192,
        }
    }
}
/// Reduce only resolution/encoding, preserving the complete image and aspect ratio.
/// The host retains the original Resource and passes its citation as `name`.
/// Unknown, enormous or unfit images fail closed instead of sending full originals.
pub fn prepare_inline_image_evidence(
    bytes: &[u8],
    name: Option<String>,
    options: ImageEvidenceOptions,
) -> Result<MediaContent, AgentError> {
    let invalid = || {
        AgentError::InvalidConfiguration("image evidence cannot be prepared within limits".into())
    };
    if bytes.is_empty()
        || bytes.len() > 20 * 1024 * 1024
        || !(128..=2048).contains(&options.max_side)
        || !(1024..=64 * 1024).contains(&options.max_encoded_bytes)
    {
        return Err(invalid());
    }
    let format = image::guess_format(bytes).map_err(|_| invalid())?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        return Err(invalid());
    }
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| invalid())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 16 * 1024 * 1024 {
        return Err(invalid());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let original = reader.decode().map_err(|_| invalid())?;
    let mut sides = vec![
        options.max_side,
        options.max_side.min(512),
        options.max_side.min(384),
    ];
    sides.dedup();
    for side in sides {
        let thumb = original.thumbnail(side, side).to_rgba8();
        // Transparent source charts are composited on white, never silently black.
        let rgb = image::RgbImage::from_fn(thumb.width(), thumb.height(), |x, y| {
            let p = thumb.get_pixel(x, y);
            let a = u32::from(p[3]);
            image::Rgb([
                ((u32::from(p[0]) * a + 255 * (255 - a)) / 255) as u8,
                ((u32::from(p[1]) * a + 255 * (255 - a)) / 255) as u8,
                ((u32::from(p[2]) * a + 255 * (255 - a)) / 255) as u8,
            ])
        });
        for quality in [70, 50, 40] {
            let mut encoded = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
                .encode_image(&rgb)
                .map_err(|_| invalid())?;
            if encoded.len() > options.max_encoded_bytes {
                continue;
            }
            let data = base64::engine::general_purpose::STANDARD.encode(encoded);
            let estimate = data
                .len()
                .saturating_add("image/jpeg".len())
                .saturating_add(2048);
            let name = name.as_ref().map(|source| {
                format!(
                    "{source} [SDK resized evidence {}x{} from {width}x{height}]",
                    rgb.width(),
                    rgb.height()
                )
            });
            return Ok(MediaContent {
                mime_type: "image/jpeg".into(),
                source: MediaSource::Base64(data),
                name,
                estimated_tokens: Some(estimate),
            });
        }
    }
    Err(invalid())
}
