use std::io::Cursor;

use image::imageops::{overlay, resize, FilterType};
use image::{
    ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb, RgbImage,
};

use khive_runtime::RuntimeError;

pub(crate) const MAX_SOURCE_SIDE: u32 = 8192;
pub(crate) const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// Post-decode working buffers per pixel. Decoder `Limits` bound only the
/// decoder's own allocations; the conversion and compositing buffers are
/// invisible to them, so admission is enforced from the header dimensions
/// and the decoded color format before any full-raster buffer exists.
pub(crate) const RGBA_BYTES_PER_PIXEL: u64 = 4;
pub(crate) const RGB_BYTES_PER_PIXEL: u64 = 3;
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

    let header_decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!("moodboard.ingest cannot identify raster: {error}"))
        })?
        .into_decoder()
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "moodboard.ingest cannot read raster header: {error}"
            ))
        })?;
    let (header_width, header_height) = header_decoder.dimensions();
    let decoded_color = header_decoder.color_type();
    drop(header_decoder);
    post_decode_admission(header_width, header_height, decoded_color)?;

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

    // `into_rgba8` reuses the decoded buffer only when it is already RGBA8;
    // for every other decode format it copies, so the decoded and RGBA
    // buffers coexist. Admission budgets for that peak per pixel; the
    // explicit drop below releases the RGBA buffer before the resize
    // allocates.
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

/// Peak post-decode working bytes per pixel for a raster decoding to
/// `color_type`: the decoded buffer coexists with the RGBA8 conversion copy
/// (`into_rgba8` reuses the buffer only for RGBA8 sources), and the RGBA
/// buffer later coexists with the matted RGB buffer. The larger phase
/// bounds the request.
fn post_decode_working_bytes_per_pixel(color_type: ColorType) -> u64 {
    let decoded = u64::from(color_type.bytes_per_pixel());
    let conversion_peak = if color_type == ColorType::Rgba8 {
        RGBA_BYTES_PER_PIXEL
    } else {
        decoded + RGBA_BYTES_PER_PIXEL
    };
    conversion_peak.max(RGBA_BYTES_PER_PIXEL + RGB_BYTES_PER_PIXEL)
}

/// Admission for the post-decode working set, judged from the header
/// dimensions and decoded color format alone. Pure and allocation-free on
/// purpose, exactly so the rejection boundary is unit-testable without
/// materializing the buffers it refuses — an over-budget raster is refused
/// before decode, so no full-raster allocation ever backs the check.
/// Arithmetic overflow on attacker-controlled dimensions fails closed.
fn post_decode_admission(
    width: u32,
    height: u32,
    color_type: ColorType,
) -> Result<(), RuntimeError> {
    let bytes_per_pixel = post_decode_working_bytes_per_pixel(color_type);
    match u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
    {
        Some(required) if required <= MAX_DECODE_ALLOC => Ok(()),
        _ => Err(RuntimeError::InvalidInput(format!(
            "moodboard.ingest raster {width}x{height} at {bytes_per_pixel} bytes per pixel \
             exceeds the {MAX_DECODE_ALLOC}-byte post-decode working memory budget"
        ))),
    }
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
        let bytes_per_pixel = post_decode_working_bytes_per_pixel(ColorType::Rgb8);
        assert_eq!(bytes_per_pixel, 7);
        let budget_pixels = MAX_DECODE_ALLOC / bytes_per_pixel;
        assert!(u64::from(u32::MAX) >= budget_pixels);

        // Exactly at the budget: admitted.
        post_decode_admission(1, budget_pixels as u32, ColorType::Rgb8)
            .expect("a raster exactly at the working-set budget is admitted");
        // One pixel past the budget: refused, naming the post-decode class.
        let error = post_decode_admission(1, budget_pixels as u32 + 1, ColorType::Rgb8)
            .expect_err("one pixel past the budget must be refused");
        assert!(error.to_string().contains("post-decode working memory"));

        // The decoder's own per-side caps admit 8192x8192, whose working set
        // (7 bytes/pixel) exceeds the budget — the arm that motivated this
        // admission must be refused here.
        post_decode_admission(MAX_SOURCE_SIDE, MAX_SOURCE_SIDE, ColorType::Rgb8)
            .expect_err("a max-side square raster exceeds the post-decode budget");

        // Attacker-controlled header dimensions fail closed on arithmetic
        // overflow instead of panicking in checked builds or wrapping in
        // release builds.
        post_decode_admission(u32::MAX, u32::MAX, ColorType::Rgba16)
            .expect_err("dimension overflow is refused, never wrapped or panicked");
    }

    // The working-set budget follows the decoded format: 16-bit rasters
    // decode into wider buffers that coexist with the RGBA8 conversion copy,
    // so their per-pixel peak exceeds the 8-bit figure.
    #[test]
    fn working_set_budget_tracks_the_decoded_format() {
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::L8), 7);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::L16), 7);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::Rgb8), 7);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::Rgba8), 7);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::La16), 8);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::Rgb16), 10);
        assert_eq!(post_decode_working_bytes_per_pixel(ColorType::Rgba16), 12);
    }

    #[test]
    fn sixteen_bit_rasters_admit_against_their_wider_working_set() {
        // A real 16-bit RGB PNG reports its decoded format through the header
        // probe the admission reads, and small 16-bit rasters still flow
        // end-to-end through preprocessing.
        let image =
            image::ImageBuffer::<Rgb<u16>, Vec<u16>>::from_pixel(13, 9, Rgb([1000, 2000, 3000]));
        let mut cursor = Cursor::new(Vec::new());
        DynamicImage::ImageRgb16(image)
            .write_to(&mut cursor, ImageFormat::Png)
            .unwrap();
        let bytes = cursor.into_inner();
        let decoder = ImageReader::new(Cursor::new(bytes.as_slice()))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.color_type(), ColorType::Rgb16);
        let prepared = prepare_raster(&bytes, Some("image/png")).unwrap();
        assert_eq!((prepared.original_width, prepared.original_height), (13, 9));

        // 8192x4681 fits an 8-bit RGB working set (7 bytes/pixel) but a
        // 16-bit RGB decode coexisting with its RGBA conversion copy peaks at
        // 10 bytes/pixel and exceeds the budget: the same dimensions produce
        // different verdicts under the two formats.
        post_decode_admission(8192, 4681, ColorType::Rgb8)
            .expect("8-bit RGB at 8192x4681 fits the working-set budget");
        let error = post_decode_admission(8192, 4681, ColorType::Rgb16)
            .expect_err("16-bit RGB at 8192x4681 exceeds the working-set budget");
        assert!(error.to_string().contains("post-decode working memory"));
    }
}
