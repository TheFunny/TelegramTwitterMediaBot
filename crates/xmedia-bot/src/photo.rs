//! Pure-Rust photo processing: brings a downloaded photo within Telegram's
//! limits (width + height ≤ 10000 px, bytes ≤ 10 MiB) without ffmpeg.
//!
//! Stack: `png` (image-png) for PNG decode/encode, `zune-jpeg` for JPEG
//! decode, `fast_image_resize` (Lanczos3) for downsampling, `jpeg-encoder`
//! for JPEG output.
//!
//! Bit-depth rule: a PNG above 24 bits (32-bit RGBA or 16-bit per channel)
//! is reduced to 24-bit RGB; 24-bit and lower depths are left untouched —
//! gray stays gray, never upconverted. The only upconversion is palette
//! expansion, which resampling requires. Alpha is flattened onto white (JPEG
//! and 24-bit RGB have no alpha channel).

use std::io::Write;

use fast_image_resize as fir;
use tempfile::NamedTempFile;

/// Telegram rejects photos whose width + height exceed this limit
/// (PHOTO_INVALID_DIMENSIONS). Verified empirically: 6300x3730 (sum 10030)
/// fails, 6100x3900 (sum 10000) passes.
pub const PHOTO_MAX_DIMENSION_SUM: u32 = 10000;
/// Resize target with a safety margin so rounding cannot cross the cap.
pub const PHOTO_TARGET_DIMENSION_SUM: u32 = 9900;
/// Upload cap (bytes): files above this are not uploaded; the bot falls back
/// to a smaller media URL instead.
pub const MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024;
/// Decode budget (bytes): a larger intermediate buffer is not worth the peak
/// memory; the photo degrades to the smaller URL instead. Also the cap for
/// downloading photos in the send fallback (they must be downloaded whole).
pub(crate) const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
/// JPEG output quality (1-100).
const JPEG_QUALITY: u8 = 90;

/// What to upload for a downloaded photo.
pub enum PhotoPrep {
    /// Upload this file (the original when within limits, else the processed
    /// copy).
    Upload(NamedTempFile),
    /// The photo cannot be brought within Telegram's limits — the caller
    /// falls back to the item's smaller URL.
    UseFallback,
}

/// A decoded image buffer tagged with its channel layout.
#[derive(Debug)]
enum PixBuf {
    Gray(Vec<u8>),
    GrayAlpha(Vec<u8>),
    Rgb(Vec<u8>),
}

impl PixBuf {
    fn pixel_type(&self) -> fir::PixelType {
        match self {
            PixBuf::Gray(_) => fir::PixelType::U8,
            PixBuf::GrayAlpha(_) => fir::PixelType::U8x2,
            PixBuf::Rgb(_) => fir::PixelType::U8x3,
        }
    }

    fn into_vec(self) -> Vec<u8> {
        match self {
            PixBuf::Gray(v) | PixBuf::GrayAlpha(v) | PixBuf::Rgb(v) => v,
        }
    }
}

/// Entry point: detects the format and processes the photo if needed.
/// The caller hands in the already-downloaded bytes (they are in memory from
/// the download anyway; re-reading the temp file would double the I/O).
pub fn prepare_photo(file: NamedTempFile, bytes: &[u8]) -> Result<PhotoPrep, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        prepare_png(file, bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        prepare_jpeg(file, bytes)
    } else {
        log::warn!("photo in unsupported format; falling back to smaller media");
        Ok(PhotoPrep::UseFallback)
    }
}

/// Parses the PNG IHDR (bytes 8..26: signature + length + "IHDR" + width +
/// height + bit depth + color type).
fn parse_png_header(bytes: &[u8]) -> Option<(u32, u32, png::BitDepth, png::ColorType)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.len() < 26 {
        return None;
    }
    let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    let depth = match *bytes.get(24)? {
        1 => png::BitDepth::One,
        2 => png::BitDepth::Two,
        4 => png::BitDepth::Four,
        8 => png::BitDepth::Eight,
        16 => png::BitDepth::Sixteen,
        _ => return None,
    };
    let color = match *bytes.get(25)? {
        0 => png::ColorType::Grayscale,
        2 => png::ColorType::Rgb,
        3 => png::ColorType::Indexed,
        4 => png::ColorType::GrayscaleAlpha,
        6 => png::ColorType::Rgba,
        _ => return None,
    };
    Some((w, h, depth, color))
}

/// Output channels of a decoded frame for the given color type (post
/// STRIP_16; palette expands to RGB).
fn output_channels(color: png::ColorType) -> usize {
    match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb | png::ColorType::Indexed => 3,
        png::ColorType::Rgba => 4,
    }
}

/// The 32→24 rule: RGBA (32-bit) becomes RGB with alpha composited onto
/// white; 16-bit per channel was already stripped to 8-bit at decode.
fn flatten_rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        let a = px[3] as u32;
        for v in &px[..3] {
            // Over white: C = C*a/255 + 255*(1 - a/255).
            let v = (*v as u32 * a + 255 * (255 - a)) / 255;
            rgb.push(v.min(255) as u8);
        }
    }
    rgb
}

/// Lanczos3 downsampling via fast_image_resize.
fn resize_pix(pix: PixBuf, w: u32, h: u32, nw: u32, nh: u32) -> Result<PixBuf, String> {
    let pixel_type = pix.pixel_type();
    let src = fir::images::Image::from_vec_u8(w, h, pix.into_vec(), pixel_type)
        .map_err(|e| format!("resize input: {e}"))?;
    let mut dst = fir::images::Image::new(nw, nh, pixel_type);
    let mut resizer = fir::Resizer::new();
    let options = fir::ResizeOptions::default()
        .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Lanczos3));
    resizer
        .resize(&src, &mut dst, &options)
        .map_err(|e| format!("resize: {e}"))?;
    let buf = dst.into_vec();
    Ok(match pixel_type {
        fir::PixelType::U8 => PixBuf::Gray(buf),
        fir::PixelType::U8x2 => PixBuf::GrayAlpha(buf),
        _ => PixBuf::Rgb(buf),
    })
}

fn encode_png(out: &mut Vec<u8>, pix: &PixBuf, w: u32, h: u32) -> Result<(), png::EncodingError> {
    let (color, buf) = match pix {
        PixBuf::Gray(v) => (png::ColorType::Grayscale, v.as_slice()),
        PixBuf::GrayAlpha(v) => (png::ColorType::GrayscaleAlpha, v.as_slice()),
        PixBuf::Rgb(v) => (png::ColorType::Rgb, v.as_slice()),
    };
    let mut encoder = png::Encoder::new(out, w, h);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(buf)?;
    Ok(())
}

fn encode_jpeg(pix: &PixBuf, w: u32, h: u32) -> Result<Vec<u8>, String> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut out = Vec::new();
    let encoder = Encoder::new(&mut out, JPEG_QUALITY);
    match pix {
        PixBuf::Gray(v) => encoder
            .encode(v, w as u16, h as u16, ColorType::Luma)
            .map_err(|e| format!("jpeg encode: {e}"))?,
        PixBuf::GrayAlpha(v) => {
            // JPEG has no alpha: composite onto white, output as gray.
            let gray: Vec<u8> = v
                .chunks_exact(2)
                .map(|px| {
                    let (g, a) = (px[0] as u32, px[1] as u32);
                    ((g * a + 255 * (255 - a)) / 255).min(255) as u8
                })
                .collect();
            encoder
                .encode(&gray, w as u16, h as u16, ColorType::Luma)
                .map_err(|e| format!("jpeg encode: {e}"))?;
        }
        PixBuf::Rgb(v) => encoder
            .encode(v, w as u16, h as u16, ColorType::Rgb)
            .map_err(|e| format!("jpeg encode: {e}"))?,
    }
    Ok(out)
}

fn write_temp(bytes: &[u8], ext: &str) -> Result<NamedTempFile, String> {
    let mut file = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .map_err(|e| format!("temp file failed: {e}"))?;
    file.as_file_mut()
        .write_all(bytes)
        .map_err(|e| format!("temp file write failed: {e}"))?;
    Ok(file)
}

fn target_dims(w: u32, h: u32) -> (u32, u32) {
    let scale = PHOTO_TARGET_DIMENSION_SUM as f64 / (w + h) as f64;
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// PNG branch: decode (16→8, palette→RGB; gray/GA stay), flatten RGBA to
/// RGB, Lanczos-downscale beyond the dimension cap, encode PNG — a PNG still
/// over the upload cap afterwards becomes JPEG.
fn prepare_png(file: NamedTempFile, bytes: &[u8]) -> Result<PhotoPrep, String> {
    let (w, h, _bit_depth, color_type) = parse_png_header(bytes).ok_or("invalid PNG header")?;
    let size_over = bytes.len() as u64 > MAX_UPLOAD_BYTES;
    if w + h <= PHOTO_MAX_DIMENSION_SUM && !size_over {
        return Ok(PhotoPrep::Upload(file));
    }
    log::debug!(
        "photo {w}x{h} ({_bit_depth:?} {color_type:?}, {} bytes) needs processing",
        bytes.len()
    );

    let channels = output_channels(color_type);
    if (w as u64) * (h as u64) * channels as u64 > MAX_DECODE_BYTES {
        log::warn!("photo decode buffer exceeds the memory budget; falling back to smaller media");
        return Ok(PhotoPrep::UseFallback);
    }

    // STRIP_16 drops 16-bit to 8-bit (the depth-reduction step); palette
    // expands to RGB (resampling requires it). Gray and gray-alpha are kept.
    let transforms = match color_type {
        png::ColorType::Indexed => png::Transformations::EXPAND,
        _ => png::Transformations::STRIP_16,
    };
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(transforms);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png decode: {e}"))?;
    let out_w = reader.info().width;
    let out_h = reader.info().height;
    let mut buf = vec![
        0u8;
        reader
            .output_buffer_size()
            .ok_or("png output buffer size")?
    ];
    reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png frame: {e}"))?;

    let mut pix = match color_type {
        png::ColorType::Rgba => PixBuf::Rgb(flatten_rgba_to_rgb(&buf)),
        png::ColorType::Grayscale => PixBuf::Gray(buf),
        png::ColorType::GrayscaleAlpha => PixBuf::GrayAlpha(buf),
        png::ColorType::Rgb | png::ColorType::Indexed => PixBuf::Rgb(buf),
    };

    let (mut w, mut h) = (out_w, out_h);
    if w + h > PHOTO_MAX_DIMENSION_SUM {
        let (nw, nh) = target_dims(w, h);
        pix = resize_pix(pix, w, h, nw, nh)?;
        (w, h) = (nw, nh);
        log::debug!("downscaled photo to {w}x{h} (Lanczos3)");
    }

    let mut png_bytes = Vec::new();
    encode_png(&mut png_bytes, &pix, w, h).map_err(|e| format!("png encode: {e}"))?;
    if png_bytes.len() as u64 <= MAX_UPLOAD_BYTES {
        return Ok(PhotoPrep::Upload(write_temp(&png_bytes, "png")?));
    }
    log::debug!("PNG still over the upload cap after processing; transcoding to JPEG");
    let jpeg_bytes = encode_jpeg(&pix, w, h)?;
    if jpeg_bytes.len() as u64 <= MAX_UPLOAD_BYTES {
        return Ok(PhotoPrep::Upload(write_temp(&jpeg_bytes, "jpg")?));
    }
    log::warn!("processed photo still exceeds the upload cap; falling back to smaller media");
    Ok(PhotoPrep::UseFallback)
}

/// JPEG branch: zune-jpeg decode → Lanczos downscale → jpeg-encoder output.
fn prepare_jpeg(file: NamedTempFile, bytes: &[u8]) -> Result<PhotoPrep, String> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
    // Decodes to RGB by default. Headers first so dimensions are known before
    // the (potentially huge) pixel decode.
    decoder
        .decode_headers()
        .map_err(|e| format!("jpeg headers: {e}"))?;
    let info = decoder.info().ok_or("jpeg info unavailable")?;
    let (w, h) = (info.width as u32, info.height as u32);
    let size_over = bytes.len() as u64 > MAX_UPLOAD_BYTES;
    if w + h <= PHOTO_MAX_DIMENSION_SUM && !size_over {
        return Ok(PhotoPrep::Upload(file));
    }
    if (w as u64) * (h as u64) * 3 > MAX_DECODE_BYTES {
        log::warn!("photo decode buffer exceeds the memory budget; falling back to smaller media");
        return Ok(PhotoPrep::UseFallback);
    }
    let pixels = decoder.decode().map_err(|e| format!("jpeg decode: {e}"))?;
    let mut pix = PixBuf::Rgb(pixels);
    let (mut w, mut h) = (w, h);
    if w + h > PHOTO_MAX_DIMENSION_SUM {
        let (nw, nh) = target_dims(w, h);
        pix = resize_pix(pix, w, h, nw, nh)?;
        (w, h) = (nw, nh);
        log::debug!("downscaled jpeg to {w}x{h} (Lanczos3)");
    }
    let jpeg_bytes = encode_jpeg(&pix, w, h)?;
    if jpeg_bytes.len() as u64 <= MAX_UPLOAD_BYTES {
        return Ok(PhotoPrep::Upload(write_temp(&jpeg_bytes, "jpg")?));
    }
    log::warn!("processed photo still exceeds the upload cap; falling back to smaller media");
    Ok(PhotoPrep::UseFallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(w: u32, h: u32, depth: u8, color: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        bytes.extend(w.to_be_bytes());
        bytes.extend(h.to_be_bytes());
        bytes.extend([depth, color, 0, 0, 0]);
        bytes
    }

    #[test]
    fn parses_png_header() {
        let bytes = png_header(8979, 5316, 16, 6); // 16-bit RGBA
        let (w, h, depth, color) = parse_png_header(&bytes).unwrap();
        assert_eq!((w, h), (8979, 5316));
        assert_eq!(depth, png::BitDepth::Sixteen);
        assert_eq!(color, png::ColorType::Rgba);

        let (_, _, depth, color) = parse_png_header(&png_header(10, 10, 8, 0)).unwrap();
        assert_eq!(depth, png::BitDepth::Eight);
        assert_eq!(color, png::ColorType::Grayscale);

        assert!(parse_png_header(b"not a png").is_none());
    }

    #[test]
    fn flatten_rgba_to_rgb_composites_over_white() {
        // opaque red stays red
        assert_eq!(flatten_rgba_to_rgb(&[255, 0, 0, 255]), vec![255, 0, 0]);
        // fully transparent → white
        assert_eq!(flatten_rgba_to_rgb(&[0, 0, 0, 0]), vec![255, 255, 255]);
        // half alpha red → (255+255)/2 = 255, (0*128 + 255*127)/255 = 127
        let out = flatten_rgba_to_rgb(&[255, 0, 0, 128]);
        assert_eq!(out[0], 255);
        assert_eq!(out[1], 127);
        assert_eq!(out[2], 127);
    }

    #[test]
    fn target_dims_stay_under_the_cap() {
        for (w, h) in [(12000u32, 7000u32), (10000, 10000), (8979, 5316)] {
            let (nw, nh) = target_dims(w, h);
            assert!(nw + nh <= PHOTO_MAX_DIMENSION_SUM, "{w}x{h} -> {nw}x{nh}");
            assert!(nw >= 1 && nh >= 1);
        }
        // already within limits: no change expected from the caller, but the
        // helper must not produce zero dimensions.
        let (nw, nh) = target_dims(500, 400);
        assert!(nw >= 1 && nh >= 1);
    }

    #[test]
    fn resize_pix_changes_dimensions() {
        // 300x200 RGB → 100x66
        let buf: Vec<u8> = (0..300 * 200 * 3).map(|i| (i % 251) as u8).collect();
        let resized = resize_pix(PixBuf::Rgb(buf), 300, 200, 100, 66).unwrap();
        match resized {
            PixBuf::Rgb(v) => assert_eq!(v.len(), 100 * 66 * 3),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    #[test]
    fn png_encode_roundtrip_keeps_gray() {
        let gray = vec![128u8; 4 * 4];
        let mut out = Vec::new();
        encode_png(&mut out, &PixBuf::Gray(gray), 4, 4).unwrap();
        assert!(!out.is_empty());
        let (_, _, depth, color) = parse_png_header(&out).unwrap();
        assert_eq!(depth, png::BitDepth::Eight);
        assert_eq!(color, png::ColorType::Grayscale);
    }

    #[test]
    fn jpeg_encode_produces_bytes() {
        let rgb = vec![128u8; 8 * 8 * 3];
        let out = encode_jpeg(&PixBuf::Rgb(rgb), 8, 8).unwrap();
        assert!(out.len() > 100);
        assert!(out.starts_with(&[0xFF, 0xD8]));
    }

    /// Writes a small dimension-oversized PNG (9999x2 → sum 10001) to a temp
    /// file and runs the full pipeline.
    fn run_pipeline(w: u32, h: u32, color: png::ColorType, fill: u8) -> Result<PhotoPrep, String> {
        let (channels, data): (usize, Vec<u8>) = match color {
            png::ColorType::Grayscale => (1, vec![fill; (w * h) as usize]),
            png::ColorType::Rgb => (3, vec![fill; (w * h * 3) as usize]),
            _ => unreachable!(),
        };
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, w, h);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&data).unwrap();
        }
        assert_eq!(data.len(), channels * (w * h) as usize);

        let mut file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        std::io::Write::write_all(file.as_file_mut(), &bytes).unwrap();
        prepare_photo(file, &bytes)
    }

    #[test]
    fn pipeline_downscales_oversized_png_keeping_format() {
        let prep = run_pipeline(9999, 2, png::ColorType::Rgb, 128).unwrap();
        match prep {
            PhotoPrep::Upload(file) => {
                let out = std::fs::read(file.path()).unwrap();
                let (w, h, depth, color) = parse_png_header(&out).unwrap();
                assert!(w + h <= PHOTO_MAX_DIMENSION_SUM, "{w}x{h}");
                assert_eq!(depth, png::BitDepth::Eight);
                assert_eq!(color, png::ColorType::Rgb);
            }
            PhotoPrep::UseFallback => panic!("over-dimension PNG should have been resized"),
        }
    }

    #[test]
    fn pipeline_keeps_gray_png_gray() {
        let prep = run_pipeline(9999, 2, png::ColorType::Grayscale, 200).unwrap();
        match prep {
            PhotoPrep::Upload(file) => {
                let out = std::fs::read(file.path()).unwrap();
                let (_, _, _, color) = parse_png_header(&out).unwrap();
                assert_eq!(color, png::ColorType::Grayscale, "gray must not upconvert");
            }
            PhotoPrep::UseFallback => panic!("over-dimension gray PNG should have been resized"),
        }
    }

    #[test]
    fn pipeline_resizes_oversized_jpeg() {
        // Build a small over-dimension JPEG with jpeg-encoder.
        let (w, h) = (9999u16, 2u16);
        let rgb = vec![90u8; (w as usize) * (h as usize) * 3];
        let mut bytes = Vec::new();
        {
            let encoder = jpeg_encoder::Encoder::new(&mut bytes, 90);
            encoder
                .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
                .unwrap();
        }
        let mut file = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        std::io::Write::write_all(file.as_file_mut(), &bytes).unwrap();
        match prepare_photo(file, &bytes).unwrap() {
            PhotoPrep::Upload(file) => {
                let out = std::fs::read(file.path()).unwrap();
                assert!(out.starts_with(&[0xFF, 0xD8]), "output must stay jpeg");
                // 9999x2 downscaled: the buffer length tells the new dims.
                assert!(out.len() > 100);
            }
            PhotoPrep::UseFallback => panic!("over-dimension JPEG should have been resized"),
        }
    }

    #[test]
    #[ignore = "heavy: generates a >10 MiB PNG (run explicitly)"]
    fn pipeline_transcodes_oversized_png_to_jpeg() {
        // 6000x4000 (sum 10000 — under the dimension cap) smooth gradient with
        // small per-pixel noise: PNG-incompressible (delta filters defeated)
        // but JPEG-friendly (DCT smooths the small noise). Verified with
        // ffmpeg: 8000x6000 amp-5 variant is a 59 MB PNG / 3.3 MB JPEG.
        let (w, h) = (6000u32, 4000u32);
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut data = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let base = (x + y) * 255 / (w + h);
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let n = ((rng >> 33) % 11) as i32 - 5; // noise in [-5, 5]
                let v = (base as i32 + n).clamp(0, 255) as u8;
                data.extend_from_slice(&[v, v, v]);
            }
        }
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, w, h);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&data).unwrap();
        }
        assert!(
            bytes.len() as u64 > MAX_UPLOAD_BYTES,
            "test needs a >10MiB PNG, got {}",
            bytes.len()
        );

        let mut file = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        std::io::Write::write_all(file.as_file_mut(), &bytes).unwrap();
        match prepare_photo(file, &bytes).unwrap() {
            PhotoPrep::Upload(file) => {
                let out = std::fs::read(file.path()).unwrap();
                assert!(out.starts_with(&[0xFF, 0xD8]), "must transcode to JPEG");
                assert!(out.len() as u64 <= MAX_UPLOAD_BYTES);
            }
            PhotoPrep::UseFallback => panic!("PNG over the byte cap must transcode to JPEG"),
        }
    }
}
