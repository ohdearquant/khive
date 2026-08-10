use std::io::Cursor;

use image::imageops::{overlay, resize, FilterType};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, Rgb, RgbImage};

use khive_runtime::RuntimeError;

pub(crate) const MAX_SOURCE_SIDE: u32 = 8192;
pub(crate) const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// RGBA (4) + matted RGB (3) working buffers per pixel. Decoder `Limits`
/// bound only the decoder's own allocations; these post-decode buffers are
/// invisible to them, so admission is enforced against this factor from the
/// header dimensions before any full-raster buffer exists.
pub(crate) const POST_DECODE_BYTES_PER_PIXEL: u64 = 7;
pub(crate) const MAX_INFERENCE_SIDE: u32 = 448;
pub(crate) const ALIGNMENT: u32 = 32;
pub(crate) const MATTE: Rgb<u8> = Rgb([128, 128, 128]);

#[derive(Debug)]
pub(crate) struct PreparedRaster {
    pub inference_png: Vec<u8>,
    pub media_type: &'static str,
    pub original_width: u32,
    pub original_height: u32,
}

pub(crate) fn prepare_raster(
    bytes: &[u8],
    declared_media_type: Option<&str>,
) -> Result<PreparedRaster, RuntimeError> {
    if bytes.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "moodboard.ingest image_base64 decodes to no bytes".to_string(),
        ));
    }

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!("moodboard.ingest cannot identify raster: {error}"))
        })?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let format = reader.format().ok_or_else(|| {
        RuntimeError::InvalidInput(
            "moodboard.ingest cannot identify raster format; accepted: PNG, JPEG, WebP".to_string(),
        )
    })?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => {
            return Err(RuntimeError::InvalidInput(format!(
            "moodboard.ingest raster format {format:?} is unsupported; accepted: PNG, JPEG, WebP"
        )))
        }
    };
    if let Some(declared) = declared_media_type {
        if declared != media_type {
            return Err(RuntimeError::InvalidInput(format!(
                "moodboard.ingest media_type {declared:?} does not match detected {media_type:?}"
            )));
        }
    }

    let (header_width, header_height) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!("moodboard.ingest cannot identify raster: {error}"))
        })?
        .into_dimensions()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard.ingest cannot read raster dimensions: {error}"
            ))
        })?;
    post_decode_admission(header_width, header_height)?;

    let decoded = reader.decode().map_err(|error| {
        RuntimeError::InvalidInput(format!("moodboard.ingest cannot decode raster: {error}"))
    })?;
    let original_width = decoded.width();
    let original_height = decoded.height();
    if original_width == 0 || original_height == 0 {
        return Err(RuntimeError::InvalidInput(
            "moodboard.ingest raster dimensions must be non-zero".to_string(),
        ));
    }

    // `into_rgba8` consumes the decoded image (reusing its buffer when it is
    // already RGBA8) so the decode-format buffer and the RGBA buffer never
    // coexist; the explicit drop below releases the RGBA buffer before the
    // resize allocates.
    let rgba = decoded.into_rgba8();
    let mut rgb = RgbImage::new(original_width, original_height);
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u32::from(source[3]);
        for channel in 0..3 {
            let foreground = u32::from(source[channel]);
            let background = u32::from(MATTE[channel]);
            target[channel] = ((foreground * alpha + background * (255 - alpha) + 127) / 255) as u8;
        }
    }
    drop(rgba);

    let longest = original_width.max(original_height);
    let (resized_width, resized_height) = if longest > MAX_INFERENCE_SIDE {
        let width = ((u64::from(original_width) * u64::from(MAX_INFERENCE_SIDE)
            + u64::from(longest) / 2)
            / u64::from(longest))
        .max(1) as u32;
        let height = ((u64::from(original_height) * u64::from(MAX_INFERENCE_SIDE)
            + u64::from(longest) / 2)
            / u64::from(longest))
        .max(1) as u32;
        (width, height)
    } else {
        (original_width, original_height)
    };
    let resized = if (resized_width, resized_height) == (original_width, original_height) {
        rgb
    } else {
        resize(&rgb, resized_width, resized_height, FilterType::Lanczos3)
    };

    let inference_width = resized_width.div_ceil(ALIGNMENT) * ALIGNMENT;
    let inference_height = resized_height.div_ceil(ALIGNMENT) * ALIGNMENT;
    let mut canvas = RgbImage::from_pixel(inference_width, inference_height, MATTE);
    let offset_x = i64::from((inference_width - resized_width) / 2);
    let offset_y = i64::from((inference_height - resized_height) / 2);
    overlay(&mut canvas, &resized, offset_x, offset_y);

    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(canvas)
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|error| {
            RuntimeError::Internal(format!(
                "encoding moodboard governed inference rendition: {error}"
            ))
        })?;
    Ok(PreparedRaster {
        inference_png: cursor.into_inner(),
        media_type,
        original_width,
        original_height,
    })
}

/// Admission for the post-decode working set, judged from header dimensions
/// alone. Pure and allocation-free on purpose, exactly so the rejection
/// boundary is unit-testable without materializing the buffers it refuses —
/// an over-budget raster is refused before decode, so no full-raster
/// allocation ever backs the check.
fn post_decode_admission(width: u32, height: u32) -> Result<(), RuntimeError> {
    let required = u64::from(width) * u64::from(height) * POST_DECODE_BYTES_PER_PIXEL;
    if required > MAX_DECODE_ALLOC {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.ingest raster {width}x{height} requires {required} bytes of post-decode \
             working memory, exceeding the {MAX_DECODE_ALLOC}-byte budget"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use image::{GenericImageView as _, Rgba, RgbaImage};
    use sha2::{Digest, Sha256};

    fn png(width: u32, height: u32, pixel: Rgba<u8>) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, pixel);
        let mut cursor = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, ImageFormat::Png)
            .unwrap();
        cursor.into_inner()
    }

    #[test]
    fn preprocessing_is_deterministic_aligned_and_matte_composited() {
        let source = png(13, 29, Rgba([255, 0, 0, 0]));
        let first = prepare_raster(&source, Some("image/png")).unwrap();
        let second = prepare_raster(&source, Some("image/png")).unwrap();
        assert_eq!(first.inference_png, second.inference_png);
        assert_eq!((first.original_width, first.original_height), (13, 29));

        let normalized = image::load_from_memory(&first.inference_png)
            .unwrap()
            .to_rgb8();
        assert_eq!(normalized.dimensions(), (32, 32));
        assert!(normalized.pixels().all(|pixel| *pixel == MATTE));
    }

    #[test]
    fn preprocessing_downscales_without_upscaling() {
        let large = prepare_raster(&png(900, 450, Rgba([10, 20, 30, 255])), None).unwrap();
        assert_eq!(
            image::load_from_memory(&large.inference_png)
                .unwrap()
                .dimensions(),
            (448, 224)
        );

        let small = prepare_raster(&png(28, 28, Rgba([10, 20, 30, 255])), None).unwrap();
        assert_eq!(
            image::load_from_memory(&small.inference_png)
                .unwrap()
                .dimensions(),
            (32, 32)
        );
    }

    #[test]
    fn nontrivial_alpha_downscale_has_governed_rgb_pixel_golden() {
        let mut source = RgbaImage::new(901, 457);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = Rgba([
                (x % 251) as u8,
                (y % 241) as u8,
                ((x * 3 + y * 5) % 256) as u8,
                ((x + y * 7) % 256) as u8,
            ]);
        }
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let prepared = prepare_raster(encoded.get_ref(), Some("image/png")).unwrap();
        let rgb = image::load_from_memory(&prepared.inference_png)
            .unwrap()
            .to_rgb8();
        assert_eq!(rgb.dimensions(), (448, 256));
        let digest = Sha256::digest(rgb.as_raw());
        let mut hex = String::with_capacity(64);
        for byte in digest {
            write!(&mut hex, "{byte:02x}").unwrap();
        }
        assert_eq!(
            hex,
            "10ca500adab8d1a97ec67a54f1ebd9112f84ce6e4a11a9c09f4226807dea19b4"
        );
    }

    #[test]
    fn declared_media_type_must_match_detected_bytes() {
        let error = prepare_raster(&png(28, 28, Rgba([10, 20, 30, 255])), Some("image/jpeg"))
            .expect_err("mismatch must fail");
        assert!(error.to_string().contains("does not match"));
    }

    // Tested at the pure boundary, like the blob-store floor check: exercising
    // the rejection through a real decode would require materializing the very
    // buffers the admission exists to refuse.
    #[test]
    fn post_decode_admission_enforces_the_working_set_budget_at_the_exact_boundary() {
        let budget_pixels = MAX_DECODE_ALLOC / POST_DECODE_BYTES_PER_PIXEL;
        assert!(u64::from(u32::MAX) >= budget_pixels);

        // Exactly at the budget: admitted.
        post_decode_admission(1, budget_pixels as u32)
            .expect("a raster exactly at the working-set budget is admitted");
        // One pixel past the budget: refused, naming the post-decode class.
        let error = post_decode_admission(1, budget_pixels as u32 + 1)
            .expect_err("one pixel past the budget must be refused");
        assert!(error.to_string().contains("post-decode working memory"));

        // The decoder's own per-side caps admit 8192x8192, whose working set
        // (7 bytes/pixel) exceeds the budget — the arm that motivated this
        // admission must be refused here.
        post_decode_admission(MAX_SOURCE_SIDE, MAX_SOURCE_SIDE)
            .expect_err("a max-side square raster exceeds the post-decode budget");
    }
}
