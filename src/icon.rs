//! 表情包 → PNG 图标的转换核心。
//!
//! 流程：解码原图 → 等比缩放并用透明像素补成正方形 → 编码成 PNG。
//! 这里不碰网络也不碰 Telegram，方便单独测试。

use anyhow::{Context, Result};
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage,
    codecs::png::PngEncoder,
    imageops::{self, FilterType},
};

/// 输出图标的边长上限。贴纸本身最大就是 512，再大只是放大糊图。
pub const MAX_SIZE: u32 = 512;

/// 一次转换的产物。
pub struct Icon {
    pub bytes: Vec<u8>,
    /// 输出的边长（正方形）。
    pub size: u32,
    /// 解码出来的原图尺寸。
    pub source: (u32, u32),
}

/// 解码图片字节（webp / png / jpeg / gif）并转成方形 PNG 图标。
pub fn convert(data: &[u8]) -> Result<Icon> {
    let img = image::load_from_memory(data)
        .context("这张图解不开，可能不是静态图片格式（动态贴纸请让机器人去拿缩略图）")?;
    let source = (img.width(), img.height());

    // 小图不放大，免得糊
    let size = MAX_SIZE.min(source.0.max(source.1).max(1));
    let canvas = square_fit(&img, size);

    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(canvas.as_raw(), size, size, ExtendedColorType::Rgba8)
        .context("写入 PNG 失败")?;

    Ok(Icon {
        bytes,
        size,
        source,
    })
}

/// 等比缩放到 `size × size` 的画布内并居中，空白处保持透明。
fn square_fit(img: &DynamicImage, size: u32) -> RgbaImage {
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = f64::from(size) / f64::from(w.max(h));
    let target_w = ((f64::from(w) * scale).round() as u32).clamp(1, size);
    let target_h = ((f64::from(h) * scale).round() as u32).clamp(1, size);

    let resized = img
        .resize_exact(target_w, target_h, FilterType::Lanczos3)
        .into_rgba8();

    // RgbaImage::new 全零填充，也就是完全透明
    let mut canvas = RgbaImage::new(size, size);
    imageops::overlay(
        &mut canvas,
        &resized,
        i64::from((size - target_w) / 2),
        i64::from((size - target_h) / 2),
    );
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    /// 造一张左半边不透明红色、右半边透明的图，用来验证缩放和留白。
    fn sample(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbaImage::new(w, h);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x < w / 2 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 0, 0])
            };
        }
        DynamicImage::ImageRgba8(img)
    }

    fn png_bytes(img: &DynamicImage) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn square_input_keeps_size_and_alpha() {
        let icon = convert(&png_bytes(&sample(512, 512))).unwrap();
        assert_eq!(icon.size, 512);
        assert_eq!(icon.source, (512, 512));

        let decoded = image::load_from_memory(&icon.bytes).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (512, 512));
        assert_eq!(decoded.get_pixel(500, 256)[3], 0, "右半边应当保持透明");
        assert_eq!(decoded.get_pixel(10, 256), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn non_square_input_is_padded_not_stretched() {
        // 512×256 缩进 512 的方画布：内容占 512×256，上下各 128 行透明
        let icon = convert(&png_bytes(&sample(512, 256))).unwrap();
        let decoded = image::load_from_memory(&icon.bytes).unwrap().into_rgba8();

        assert_eq!(decoded.dimensions(), (512, 512));
        assert_eq!(decoded.get_pixel(8, 8)[3], 0, "顶部留白应当透明");
        assert_eq!(decoded.get_pixel(8, 503)[3], 0, "底部留白应当透明");
        assert_eq!(decoded.get_pixel(64, 256), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn small_source_is_not_upscaled() {
        let icon = convert(&png_bytes(&sample(128, 128))).unwrap();
        assert_eq!(icon.size, 128);
        assert_eq!(icon.source, (128, 128));
    }

    #[test]
    fn undecodable_input_is_an_error() {
        // .tgs 其实是 gzip 过的 JSON，走到这里必须报错而不是 panic
        assert!(convert(&[0x1f, 0x8b, 0x08, 0x00, 0x00]).is_err());
    }
}
