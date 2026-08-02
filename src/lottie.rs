//! 用 rlottie 渲染 Telegram 的 tgs 动态贴纸。
//!
//! tgs 是 gzip 压过的 Lottie **矢量**动画 JSON，既不是视频也不是位图，ffmpeg 不认，
//! 必须先逐帧渲染成位图再交给 GIF 那条流水线。

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io::Read,
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use rlottie::{Animation, Size, Surface};

/// 解压后的 JSON 上限，挡一下压缩炸弹。
const MAX_JSON: u64 = 32 * 1024 * 1024;

/// 单个 tgs 最多渲多少帧。Telegram 限制 tgs 不超过 3 秒，这个上限只是兜底。
const MAX_FRAMES: usize = 300;

/// 渲染出来的裸帧序列。
///
/// 像素是 **BGRA、预乘 alpha**（rlottie 用的是 ARGB32_Premultiplied），
/// 交给 ffmpeg 前要先 `unpremultiply`，否则半透明边缘会发黑。
pub struct Frames {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub count: usize,
}

/// 把 tgs 渲染成 `size × size` 的帧序列，帧率压到 `fps` 以内。
///
/// 纯 CPU 活，调用方记得放进 `spawn_blocking`。
pub fn render(tgs: &[u8], size: u32, fps: u32) -> Result<Frames> {
    let json = decompress(tgs)?;
    // rlottie 走 C 字符串，内嵌 nul 会让它 panic，先挡掉
    if json.contains(&0) {
        bail!("tgs 解压出来的内容含 nul 字节，不像正常的 Lottie 文件");
    }

    // rlottie 内部按 cache_key 缓存，key 撞了会拿到别的动画，所以按内容取
    let mut hasher = DefaultHasher::new();
    json.hash(&mut hasher);
    let cache_key = format!("tgs-{:016x}", hasher.finish());

    let mut anim = Animation::from_data(json, cache_key, "").context("rlottie 解析不了这个 tgs")?;

    let total = anim.totalframe();
    let src_fps = anim.framerate();
    if total == 0 || !src_fps.is_finite() || src_fps <= 0.0 {
        bail!("这个 tgs 没有有效的帧信息");
    }

    // 抽帧：tgs 通常是 60fps，我们只要 20
    let fps = fps.max(1).min(src_fps.round().max(1.0) as u32);
    let step = src_fps / f64::from(fps);
    let count = ((total as f64 / step).floor() as usize).clamp(1, MAX_FRAMES);

    let size = Size::new(size as usize, size as usize);
    let mut surface = Surface::new(size);
    let mut data = Vec::with_capacity(count * size.width * size.height * 4);

    for i in 0..count {
        let frame = ((i as f64 * step).round() as usize).min(total - 1);
        anim.render(frame, &mut surface);
        // BGRA 结构体的字段顺序就是内存顺序，和 ffmpeg 的 bgra 像素格式对得上
        data.extend(
            surface
                .data()
                .iter()
                .flat_map(|px| [px.b, px.g, px.r, px.a]),
        );
    }

    Ok(Frames {
        data,
        width: size.width as u32,
        height: size.height as u32,
        fps,
        count,
    })
}

/// tgs 就是 gzip 过的 JSON；顺手也接受没压过的裸 JSON。
fn decompress(tgs: &[u8]) -> Result<Vec<u8>> {
    if !tgs.starts_with(&[0x1f, 0x8b]) {
        return Ok(tgs.to_vec());
    }
    let mut out = Vec::new();
    GzDecoder::new(tgs)
        .take(MAX_JSON)
        .read_to_end(&mut out)
        .context("tgs 解压失败")?;
    if out.is_empty() {
        bail!("tgs 解压出来是空的");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn decompress_handles_both_gzipped_and_plain() {
        let json = br#"{"v":"5.5.2"}"#;
        assert_eq!(decompress(&gzip(json)).unwrap(), json);
        assert_eq!(decompress(json).unwrap(), json);
    }

    #[test]
    fn broken_gzip_is_an_error() {
        assert!(decompress(&[0x1f, 0x8b, 0x08, 0x00, 0x99]).is_err());
    }

    #[test]
    fn garbage_is_not_a_lottie() {
        assert!(render(br#"{"not":"lottie"}"#, 64, 10).is_err());
    }
}
