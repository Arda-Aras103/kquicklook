use crate::preview::PreviewContent;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageReader, ImageResult};
use resvg::{tiny_skia, usvg};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

const MAX_PREVIEW_DIM: u32 = 1920;
/// Keep animation frames smaller since all frames are held in memory.
const MAX_ANIM_DIM: u32 = 800;
/// Maximum total decoded pixel bytes for an animation (RAM cap).
const MAX_ANIM_BYTES: usize = 64 * 1024 * 1024;
const MAX_ANIM_FRAMES: usize = 500;

/// Fits dimensions within `max` while preserving aspect ratio.
fn fit_dims(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w, h);
    }
    let s = max as f32 / w.max(h) as f32;
    (((w as f32 * s) as u32).max(1), ((h as f32 * s) as u32).max(1))
}

/// Decodes an image and applies EXIF orientation (so phone photos aren't sideways).
fn decode_oriented(path: &Path) -> ImageResult<DynamicImage> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

pub fn load_raster_image(path: &Path) -> PreviewContent {
    match decode_oriented(path) {
        Ok(img) => {
            let (orig_w, orig_h) = (img.width(), img.height());
            // Downscale very large images to screen resolution to save RAM
            let processed = if orig_w > MAX_PREVIEW_DIM || orig_h > MAX_PREVIEW_DIM {
                img.resize(MAX_PREVIEW_DIM, MAX_PREVIEW_DIM, FilterType::Triangle)
            } else {
                img
            };
            let rgba_img = processed.into_rgba8();
            let (w, h) = rgba_img.dimensions();
            PreviewContent::Image {
                rgba: rgba_img.into_raw(),
                width: w as usize,
                height: h as usize,
                original_dimensions: (orig_w, orig_h),
            }
        }
        Err(err) => PreviewContent::Unsupported {
            reason: format!("Failed to read image: {err}"),
            hex_preview: String::new(),
        },
    }
}

/// Decodes animated GIFs frame by frame; falls back to static image loading for single-frame GIFs.
pub fn load_gif(path: &Path) -> PreviewContent {
    let Some(decoder) = File::open(path)
        .ok()
        .and_then(|f| image::codecs::gif::GifDecoder::new(BufReader::new(f)).ok())
    else {
        return load_raster_image(path);
    };

    let mut frames = Vec::new();
    let mut total_bytes = 0usize;
    let mut dims = (0u32, 0u32);
    let mut orig = (0u32, 0u32);

    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        let (num, den) = frame.delay().numer_denom_ms();
        let mut delay_ms = num.checked_div(den).unwrap_or(100);
        if delay_ms < 20 {
            delay_ms = 100; // Match browser behavior: very short delays default to 100 ms
        }

        let buf = frame.into_buffer();
        orig = buf.dimensions();
        let (tw, th) = fit_dims(orig.0, orig.1, MAX_ANIM_DIM);
        let buf = if (tw, th) != orig {
            image::imageops::resize(&buf, tw, th, FilterType::Triangle)
        } else {
            buf
        };
        dims = buf.dimensions();
        total_bytes += buf.len();
        frames.push((buf.into_raw(), delay_ms));

        if total_bytes >= MAX_ANIM_BYTES || frames.len() >= MAX_ANIM_FRAMES {
            break;
        }
    }

    if frames.len() <= 1 {
        return load_raster_image(path);
    }

    PreviewContent::Animation {
        frames,
        width: dims.0 as usize,
        height: dims.1 as usize,
        original_dimensions: orig,
    }
}

pub fn load_svg_image(path: &Path) -> PreviewContent {
    let Ok(svg_bytes) = std::fs::read(path) else {
        return PreviewContent::Unsupported {
            reason: "Failed to read SVG file.".to_string(),
            hex_preview: String::new(),
        };
    };

    let opt = usvg::Options::default();
    let Ok(tree) = usvg::Tree::from_data(&svg_bytes, &opt) else {
        return PreviewContent::Unsupported {
            reason: "Invalid or corrupted SVG format.".to_string(),
            hex_preview: String::new(),
        };
    };

    let size = tree.size();
    let orig_w = size.width().max(1.0) as u32;
    let orig_h = size.height().max(1.0) as u32;

    // Scale SVG to ~1200px for crisp rendering
    let target_max = 1200.0_f32;
    let scale = (target_max / size.width().max(size.height())).clamp(0.5, 4.0);
    let render_w = ((size.width() * scale) as u32).clamp(1, MAX_PREVIEW_DIM);
    let render_h = ((size.height() * scale) as u32).clamp(1, MAX_PREVIEW_DIM);

    let Some(mut pixmap) = tiny_skia::Pixmap::new(render_w, render_h) else {
        return PreviewContent::Unsupported {
            reason: "Failed to allocate SVG pixel buffer.".to_string(),
            hex_preview: String::new(),
        };
    };

    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    PreviewContent::Image {
        rgba: pixmap.take(),
        width: render_w as usize,
        height: render_h as usize,
        original_dimensions: (orig_w, orig_h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fit_dims() {
        assert_eq!(fit_dims(400, 300, 800), (400, 300));
        assert_eq!(fit_dims(1600, 800, 800), (800, 400));
        assert_eq!(fit_dims(500, 2000, 800), (200, 800));
    }

    #[test]
    fn test_exif_orientation_applied() {
        // 2x1 JPEG + EXIF Orientation=6 (90° CW) -> should decode as 1x2
        let dir = std::env::temp_dir().join("kquicklook_exif_test.jpg");
        let img = image::RgbImage::from_pixel(2, 1, image::Rgb([200, 10, 10]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&img)
            .unwrap();
        // Insert APP1/EXIF segment right after SOI (big-endian TIFF, single IFD entry)
        let exif: &[u8] = &[
            0xFF, 0xE1, 0x00, 0x22, b'E', b'x', b'i', b'f', 0, 0, b'M', b'M', 0x00, 0x2A, 0, 0,
            0, 8, 0x00, 0x01, 0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1, 0x00, 0x06, 0, 0, 0, 0, 0, 0,
        ];
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(exif);
        out.extend_from_slice(&jpeg[2..]);
        std::fs::write(&dir, &out).unwrap();

        match load_raster_image(&dir) {
            PreviewContent::Image { width, height, .. } => assert_eq!((width, height), (1, 2)),
            _ => panic!("Failed to load JPEG"),
        }
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn test_animated_gif() {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, RgbaImage};
        let path = std::env::temp_dir().join("kquicklook_anim_test.gif");
        {
            let file = File::create(&path).unwrap();
            let mut enc = GifEncoder::new(file);
            for c in [0u8, 255] {
                let buf = RgbaImage::from_pixel(4, 4, image::Rgba([c, c, c, 255]));
                enc.encode_frame(Frame::from_parts(buf, 0, 0, Delay::from_numer_denom_ms(50, 1)))
                    .unwrap();
            }
        }
        match load_gif(&path) {
            PreviewContent::Animation { frames, .. } => {
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[0].1, 50);
            }
            _ => panic!("Should have loaded as animated GIF"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
