//! Decoding a label page (PNG) into the top-down BGRA pixels GDI draws. Shared by
//! every platform so it can be tested everywhere, but only the Windows path uses it.

#![cfg_attr(not(windows), allow(dead_code))]

/// Largest side accepted, in pixels: 300 mm at 600 dpi is about 7100.
const MAX_SIDE_PX: u32 = 10_000;

/// A decoded page: `bgra` is `width * height * 4` bytes, top row first, opaque.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// White shows through anything transparent: that is what paper does.
fn over_white(channel: u8, alpha: u8) -> u8 {
    let (c, a) = (u32::from(channel), u32::from(alpha));
    ((c * a + 255 * (255 - a) + 127) / 255) as u8
}

pub fn decode_png(bytes: &[u8]) -> Result<Bitmap, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Whatever depth or palette the encoder chose, read back plain 8-bit channels.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).map_err(|e| e.to_string())?;
    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 || width > MAX_SIDE_PX || height > MAX_SIDE_PX {
        return Err(format!(
            "the image size {width} x {height} is not supported"
        ));
    }

    let samples = &buffer[..info.buffer_size()];
    let pixel_count = (width as usize) * (height as usize);
    let mut bgra = Vec::with_capacity(pixel_count * 4);
    match info.color_type {
        png::ColorType::Rgba => {
            for px in samples.chunks_exact(4) {
                bgra.extend_from_slice(&[
                    over_white(px[2], px[3]),
                    over_white(px[1], px[3]),
                    over_white(px[0], px[3]),
                    255,
                ]);
            }
        }
        png::ColorType::Rgb => {
            for px in samples.chunks_exact(3) {
                bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in samples.chunks_exact(2) {
                let grey = over_white(px[0], px[1]);
                bgra.extend_from_slice(&[grey, grey, grey, 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &grey in samples {
                bgra.extend_from_slice(&[grey, grey, grey, 255]);
            }
        }
        png::ColorType::Indexed => return Err("indexed images are not supported".into()),
    }
    if bgra.len() != pixel_count * 4 {
        return Err("the image data is incomplete".into());
    }
    Ok(Bitmap {
        width,
        height,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn rgba_becomes_top_down_bgr_with_transparency_over_white() {
        // 2x1: opaque black, then fully transparent (shows paper).
        let png = encode(2, 1, png::ColorType::Rgba, &[0, 0, 0, 255, 10, 20, 30, 0]);
        let bitmap = decode_png(&png).unwrap();
        assert_eq!((bitmap.width, bitmap.height), (2, 1));
        assert_eq!(bitmap.bgra, [0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn colour_channels_are_swapped_to_bgr() {
        let png = encode(1, 1, png::ColorType::Rgb, &[10, 20, 30]);
        assert_eq!(decode_png(&png).unwrap().bgra, [30, 20, 10, 255]);
    }

    #[test]
    fn greyscale_is_widened_and_rows_stay_top_first() {
        let png = encode(1, 2, png::ColorType::Grayscale, &[0, 255]);
        let bitmap = decode_png(&png).unwrap();
        assert_eq!(bitmap.bgra, [0, 0, 0, 255, 255, 255, 255, 255]);
        let with_alpha = encode(1, 1, png::ColorType::GrayscaleAlpha, &[0, 255]);
        assert_eq!(decode_png(&with_alpha).unwrap().bgra, [0, 0, 0, 255]);
    }

    #[test]
    fn half_transparent_black_is_mid_grey() {
        let png = encode(1, 1, png::ColorType::Rgba, &[0, 0, 0, 128]);
        let grey = decode_png(&png).unwrap().bgra[0];
        assert!((126..=129).contains(&grey), "{grey}");
    }

    #[test]
    fn damaged_or_foreign_data_is_refused() {
        assert!(decode_png(b"not a png").is_err());
        let mut png = encode(1, 1, png::ColorType::Rgb, &[1, 2, 3]);
        png.truncate(png.len() / 2);
        assert!(decode_png(&png).is_err());
    }
}
