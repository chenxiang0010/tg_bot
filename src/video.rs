//! 用 ffmpeg 的库（ffmpeg-next / libav*）把动图转成 GIF。
//!
//! 全程在内存里：输入挂一个自定义 AVIO 读回调，输出用 dyn_buf，不落任何临时文件。
//! 滤镜链沿用 ffmpeg CLI 那套字符串，交给 `avfilter_graph_parse` 解析。

use std::{ptr, slice};

use anyhow::{Context as _, Result, anyhow, bail};
use ffmpeg_next::{
    Rational, codec, decoder, encoder, ffi, filter, format::Pixel, format::context as fmt_ctx,
    frame, media, util::error::EAGAIN,
};

/// 降级阶梯 `(边长, 帧率)`：出来的 GIF 超过体积上限就往下走一档。
pub const LADDER: [(u32, u32); 4] = [(512, 20), (384, 16), (320, 15), (256, 12)];

/// 整套打包时从更小的档位起步 —— 不然一套下来几百 MB，还要反复编码。
pub const PACK_LADDER: [(u32, u32); 2] = [(320, 15), (256, 12)];

/// 单张 GIF 的体积软上限。
pub const SOFT_LIMIT: usize = 4 * 1024 * 1024;

/// 整套打包时每张的体积软上限。
pub const PACK_SOFT_LIMIT: usize = 1024 * 1024;

/// 自定义 AVIO 的缓冲区大小。
const AVIO_BUF: usize = 32 * 1024;

/// 启动时探测到的 ffmpeg 能力。
#[derive(Clone, Copy, Debug, Default)]
pub struct Ffmpeg {
    /// libav 初始化成功且 GIF 编码器在。库是静态链进来的，正常都为 true。
    pub available: bool,
    /// libvpx-vp9 解码器在不在。
    ///
    /// 关键：ffmpeg 内置的 `vp9` 解码器会把 Telegram 视频贴纸的透明通道解成全不透明，
    /// 只有 `libvpx-vp9` 能正确还原 alpha，所以 webm 必须显式选它。
    pub libvpx_vp9: bool,
}

pub struct Gif {
    pub bytes: Vec<u8>,
    pub size: u32,
    pub fps: u32,
}

/// 动图的第一帧，编码成 PNG。
pub struct Still {
    pub bytes: Vec<u8>,
    /// 实际输出的边长。小图不会被放大，所以可能比请求的 size 小。
    pub size: u32,
}

/// 送进转换流水线的输入。数据是 owned 的，方便整个转换丢进阻塞线程池。
pub enum Input {
    /// 已编码的容器（webm / mp4 / gif），让 libavformat 自己 demux。
    Encoded {
        data: Vec<u8>,
        /// 是 VP9，需要显式指定 libvpx-vp9 才能保住透明通道。
        vp9: bool,
    },
    /// rlottie 渲染出来的裸帧：BGRA、预乘 alpha。
    RawBgra {
        data: Vec<u8>,
        width: u32,
        height: u32,
        fps: u32,
    },
}

/// 进程启动时初始化 libav 并看看家伙什齐不齐。
pub fn probe() -> Ffmpeg {
    if let Err(e) = ffmpeg_next::init() {
        log::warn!("libav 初始化失败：{e}");
        return Ffmpeg::default();
    }
    Ffmpeg {
        available: encoder::find(codec::Id::GIF).is_some(),
        libvpx_vp9: decoder::find_by_name("libvpx-vp9").is_some(),
    }
}

/// 沿着阶梯往下试，返回第一个不超 `limit` 的结果；都超就返回最小的那个。
pub async fn to_gif(ff: Ffmpeg, input: Input, ladder: &[(u32, u32)], limit: usize) -> Result<Gif> {
    if !ff.available {
        bail!("libav 没初始化好，动图转不了");
    }
    let ladder = ladder.to_vec();
    // 编解码是纯 CPU 活，别占着 async 运行时
    tokio::task::spawn_blocking(move || run_ladder(ff, &input, &ladder, limit))
        .await
        .context("转换任务被中断")?
}

/// 抽动图的第一帧，编码成 PNG。
///
/// 用途：Telegram 的动图贴纸本来都带缩略图，但那是服务端的行为、没写进文档。
/// 万一没有缩略图，总得有个办法拿到一张静态图 —— 把原始 webm/tgs 丢给图片解码器
/// 是解不开的，只能靠 libav 自己抽帧。
pub async fn to_still(ff: Ffmpeg, input: Input, size: u32) -> Result<Still> {
    if !ff.available {
        bail!("libav 没初始化好，动图抽不了帧");
    }
    tokio::task::spawn_blocking(move || still(ff, &input, size))
        .await
        .context("抽帧任务被中断")?
}

/// 抽帧的滤镜链：只取第一帧 → 等比缩放（不放大）→ 透明边补成正方形。
///
/// 和 GIF 那条链的区别：不抽帧率、不做调色板，直接出 RGBA 交给 PNG 编码器。
fn still_filter_spec(size: u32) -> String {
    format!(
        "select='eq(n\\,0)',\
         scale='min(iw,{size})':'min(ih,{size})':force_original_aspect_ratio=decrease:flags=lanczos,\
         format=rgba,\
         pad='max(iw,ih)':'max(iw,ih)':(ow-iw)/2:(oh-ih)/2:color=#00000000"
    )
}

/// 从已编码的容器里抽第一帧。
fn still_encoded(ff: Ffmpeg, data: &[u8], vp9: bool, size: u32) -> Result<Vec<u8>> {
    let mut source = memio::Reader::open(data)?;

    let (stream_index, time_base, parameters) = {
        let ictx = source.context();
        let stream = ictx
            .streams()
            .best(media::Type::Video)
            .context("这个文件里没有视频流")?;
        (stream.index(), stream.time_base(), stream.parameters())
    };

    let ctx = codec::context::Context::from_parameters(parameters).context("读取流参数失败")?;
    let mut decoder = if vp9 && ff.libvpx_vp9 {
        let vpx = decoder::find_by_name("libvpx-vp9").context("找不到 libvpx-vp9")?;
        ctx.decoder()
            .open_as(vpx)
            .context("打开 libvpx-vp9 解码器失败")?
            .video()
    } else {
        ctx.decoder().video()
    }
    .context("打开解码器失败")?;

    // 滤镜图：解出来的帧 → 缩放补方 → buffersink
    let mut graph = filter::Graph::new();
    let mut first_size: Option<(u32, u32)> = None;
    let mut out_png: Option<Vec<u8>> = None;

    for (stream, packet) in source.context().packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).context("送包给解码器失败")?;
        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if first_size.is_none() {
                first_size = Some((decoded.width(), decoded.height()));
                let (w, h) = first_size.unwrap();
                let av_fmt: ffi::AVPixelFormat = decoded.format().into();
                let args = format!(
                    "video_size={w}x{h}:pix_fmt={}:time_base={}/{}:pixel_aspect=1/1",
                    av_fmt as i32,
                    time_base.numerator(),
                    time_base.denominator()
                );
                graph
                    .add(
                        &filter::find("buffer").context("没有 buffer 滤镜")?,
                        "in",
                        &args,
                    )
                    .context("创建滤镜输入失败")?;
                graph
                    .add(
                        &filter::find("buffersink").context("没有 buffersink 滤镜")?,
                        "out",
                        "",
                    )
                    .context("创建滤镜输出失败")?;
                graph
                    .output("in", 0)
                    .and_then(|p| p.input("out", 0))
                    .and_then(|p| p.parse(&still_filter_spec(size)))
                    .map_err(|e| anyhow!("解析滤镜链失败：{e}"))?;
                graph.validate().context("滤镜链校验失败")?;
            }
            graph
                .get("in")
                .context("找不到滤镜输入")?
                .source()
                .add(&decoded)
                .context("喂帧给滤镜失败")?;

            // 取滤镜吐出来的帧
            let mut filtered = frame::Video::empty();
            if graph
                .get("out")
                .context("找不到滤镜输出")?
                .sink()
                .frame(&mut filtered)
                .is_err()
            {
                continue;
            }
            out_png = Some(encode_png(&filtered)?);
            break;
        }
        if out_png.is_some() {
            break;
        }
    }

    out_png.context("动图里一帧都没解出来")
}

/// 从 PNG 的 IHDR 里读出实际宽高。
///
/// 小图不会被放大，滤镜链补方后的边长可能比请求的小，
/// 所以尺寸得从实际产出的字节里读，而不是用请求值。
fn png_size(bytes: &[u8]) -> Result<(u32, u32)> {
    const IHDR: &[u8] = b"IHDR";
    if bytes.len() < 24 || &bytes[12..16] != IHDR {
        bail!("这不是 PNG");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    Ok((width, height))
}

/// 把一帧 RGBA 编码成 PNG。
fn encode_png(frame: &frame::Video) -> Result<Vec<u8>> {
    let codec = encoder::find(codec::Id::PNG).context("没有 PNG 编码器")?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .context("创建 PNG 编码器失败")?;
    enc.set_width(frame.width());
    enc.set_height(frame.height());
    enc.set_format(Pixel::RGBA);
    enc.set_time_base(Rational::new(1, 1));
    let mut encoder = enc.open_as(codec).context("打开 PNG 编码器失败")?;

    let (octx, dyn_buf) = memio::open_output("image2pipe")?;
    let mut octx = octx;
    let mut stream = octx.add_stream(codec).context("给 PNG 加输出流失败")?;
    stream.set_parameters(&encoder);
    stream.set_time_base(Rational::new(1, 1));
    octx.write_header().context("写 PNG 文件头失败")?;

    let mut packet = ffmpeg_next::Packet::empty();
    let send_frame = frame.clone();
    encoder
        .send_frame(&send_frame)
        .context("送帧给 PNG 编码器失败")?;
    encoder.send_eof().context("通知 PNG 编码器收尾失败")?;

    let mut data = Vec::new();
    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {}
            Err(ffmpeg_next::Error::Other { errno }) if errno == EAGAIN => break,
            Err(ffmpeg_next::Error::Eof) => break,
            Err(e) => bail!("取 PNG 编码结果失败：{e}"),
        }
        packet.set_stream(0);
        packet.write_interleaved(&mut octx).context("写 PNG 失败")?;
    }
    octx.write_trailer().context("写 PNG 文件尾失败")?;

    // dyn_buf 只能用 avio_close_dyn_buf 关；关完把 pb 置空，
    // 否则 Output 的 drop 会再 close 一次。
    let mut buf: *mut u8 = ptr::null_mut();
    let len = unsafe { ffi::avio_close_dyn_buf(dyn_buf, &mut buf) };
    unsafe { (*octx.as_mut_ptr()).pb = ptr::null_mut() };
    if len <= 0 || buf.is_null() {
        if !buf.is_null() {
            unsafe { ffi::av_free(buf.cast()) };
        }
        bail!("PNG 输出是空的");
    }
    let bytes = unsafe { slice::from_raw_parts(buf, len as usize).to_vec() };
    unsafe { ffi::av_free(buf.cast()) };
    data.extend_from_slice(&bytes);
    Ok(data)
}

/// 动图抽第一帧成 PNG。
fn still(ff: Ffmpeg, input: &Input, size: u32) -> Result<Still> {
    let bytes = match input {
        Input::Encoded { data, vp9 } => still_encoded(ff, data, *vp9, size)?,
        Input::RawBgra {
            data,
            width,
            height,
            fps: src_fps,
        } => {
            // rlottie 的裸帧：直接取第一帧，走 GIF 那条链的解码逻辑再抽帧太绕，
            // 这里自己取第一块 BGRA 交给滤镜图
            let width = *width;
            let height = *height;
            let stride = width as usize * 4;
            let frame_bytes = stride * height as usize;
            if frame_bytes == 0 || data.len() < frame_bytes {
                bail!("裸帧数据长度 {} 和 {width}×{height} 对不上", data.len());
            }
            let mut graph = filter::Graph::new();
            let av_fmt: ffi::AVPixelFormat = Pixel::BGRA.into();
            let tb = Rational::new(1, (*src_fps).max(1) as i32);
            let args = format!(
                "video_size={width}x{height}:pix_fmt={}:time_base={}/{}:pixel_aspect=1/1",
                av_fmt as i32,
                tb.numerator(),
                tb.denominator()
            );
            graph
                .add(
                    &filter::find("buffer").context("没有 buffer 滤镜")?,
                    "in",
                    &args,
                )
                .context("创建滤镜输入失败")?;
            graph
                .add(
                    &filter::find("buffersink").context("没有 buffersink 滤镜")?,
                    "out",
                    "",
                )
                .context("创建滤镜输出失败")?;
            graph
                .output("in", 0)
                .and_then(|p| p.input("out", 0))
                .and_then(|p| p.parse(&still_filter_spec(size)))
                .map_err(|e| anyhow!("解析滤镜链失败：{e}"))?;
            graph.validate().context("滤镜链校验失败")?;

            let mut frame = frame::Video::new(Pixel::BGRA, width, height);
            let dst_stride = frame.stride(0);
            {
                let dst = frame.data_mut(0);
                for row in 0..height as usize {
                    let from = row * stride;
                    dst[row * dst_stride..row * dst_stride + stride]
                        .copy_from_slice(&data[from..from + stride]);
                }
            }
            frame.set_pts(Some(0));
            graph
                .get("in")
                .context("找不到滤镜输入")?
                .source()
                .add(&frame)
                .context("喂帧给滤镜失败")?;

            let mut filtered = frame::Video::empty();
            graph
                .get("out")
                .context("找不到滤镜输出")?
                .sink()
                .frame(&mut filtered)
                .context("滤镜没吐出帧")?;
            encode_png(&filtered)?
        }
    };
    // 尺寸从实际产出的 PNG 里读：小图不放大，补方后可能比请求的小
    let (width, height) = png_size(&bytes)?;
    Ok(Still {
        bytes,
        size: width.max(height),
    })
}

fn run_ladder(ff: Ffmpeg, input: &Input, ladder: &[(u32, u32)], limit: usize) -> Result<Gif> {
    let mut smallest: Option<Gif> = None;
    for &(size, fps) in ladder {
        let bytes = convert(ff, input, size, fps)?;
        let gif = Gif { size, fps, bytes };
        if gif.bytes.len() <= limit {
            return Ok(gif);
        }
        log::debug!(
            "GIF {size}px/{fps}fps 体积 {} 超过上限，降一档",
            gif.bytes.len()
        );
        smallest = Some(gif);
    }
    smallest.context("没有产出任何结果")
}

fn convert(ff: Ffmpeg, input: &Input, size: u32, fps: u32) -> Result<Vec<u8>> {
    match input {
        Input::Encoded { data, vp9 } => convert_encoded(ff, data, *vp9, size, fps),
        Input::RawBgra {
            data,
            width,
            height,
            fps: src_fps,
        } => convert_raw(data, *width, *height, *src_fps, size, fps),
    }
}

/// 已编码容器：自定义 AVIO 读内存 → demux → 解码 → 滤镜 → GIF。
fn convert_encoded(ff: Ffmpeg, data: &[u8], vp9: bool, size: u32, fps: u32) -> Result<Vec<u8>> {
    let mut source = memio::Reader::open(data)?;

    // 先把要用的信息取成 owned，免得后面 demux 时还借着 ictx
    let (stream_index, time_base, parameters) = {
        let ictx = source.context();
        let stream = ictx
            .streams()
            .best(media::Type::Video)
            .context("这个文件里没有视频流")?;
        (stream.index(), stream.time_base(), stream.parameters())
    };

    let ctx = codec::context::Context::from_parameters(parameters).context("读取流参数失败")?;
    let mut decoder = if vp9 && ff.libvpx_vp9 {
        let vpx = decoder::find_by_name("libvpx-vp9").context("找不到 libvpx-vp9")?;
        ctx.decoder()
            .open_as(vpx)
            .context("打开 libvpx-vp9 解码器失败")?
            .video()
    } else {
        ctx.decoder().video()
    }
    .context("打开解码器失败")?;

    let mut pipeline: Option<Pipeline> = None;
    let mut decoded = frame::Video::empty();

    // 宏懒得写，用闭包又要同时可变借 pipeline，索性摊开
    for (stream, packet) in source.context().packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).context("送包给解码器失败")?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            let pipe = match pipeline {
                Some(ref mut p) => p,
                None => pipeline.insert(Pipeline::new(
                    decoded.width(),
                    decoded.height(),
                    decoded.format(),
                    time_base,
                    size,
                    fps,
                    false,
                )?),
            };
            pipe.push(&mut decoded)?;
        }
    }

    decoder.send_eof().context("通知解码器收尾失败")?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        let pipe = match pipeline {
            Some(ref mut p) => p,
            None => pipeline.insert(Pipeline::new(
                decoded.width(),
                decoded.height(),
                decoded.format(),
                time_base,
                size,
                fps,
                false,
            )?),
        };
        pipe.push(&mut decoded)?;
    }

    pipeline.context("一帧都没解出来")?.finish()
}

/// rlottie 的裸帧：不用 demux 也不用解码，直接造帧喂滤镜。
fn convert_raw(
    data: &[u8],
    width: u32,
    height: u32,
    src_fps: u32,
    size: u32,
    fps: u32,
) -> Result<Vec<u8>> {
    let stride = width as usize * 4;
    let frame_bytes = stride * height as usize;
    if frame_bytes == 0 || !data.len().is_multiple_of(frame_bytes) {
        bail!("裸帧数据长度 {} 和 {width}×{height} 对不上", data.len());
    }

    let time_base = Rational::new(1, src_fps.max(1) as i32);
    let mut pipeline = Pipeline::new(width, height, Pixel::BGRA, time_base, size, fps, true)?;

    for (i, chunk) in data.chunks_exact(frame_bytes).enumerate() {
        let mut frame = frame::Video::new(Pixel::BGRA, width, height);
        // libav 的行间可能有 padding，得按 stride 逐行拷
        let dst_stride = frame.stride(0);
        let dst = frame.data_mut(0);
        for row in 0..height as usize {
            let from = row * stride;
            dst[row * dst_stride..row * dst_stride + stride]
                .copy_from_slice(&chunk[from..from + stride]);
        }
        frame.set_pts(Some(i as i64));
        pipeline.push(&mut frame)?;
    }

    pipeline.finish()
}

/// 滤镜图 + GIF 编码 + 封装，一条龙。
struct Pipeline {
    graph: filter::Graph,
    /// 编码器要等 palettegen 在 EOF 时吐出调色板后才知道输出规格，所以延迟到抽帧阶段再建。
    out: Option<Encoded>,
    fps: u32,
    /// 最近两帧的 pts。收尾时要告诉滤镜 EOF 在什么时间点，
    /// 而末帧该持续多久只能按相邻帧的间隔推算。
    last_pts: Option<i64>,
    prev_pts: Option<i64>,
}

struct Encoded {
    encoder: encoder::video::Encoder,
    octx: fmt_ctx::Output,
    dyn_buf: *mut ffi::AVIOContext,
    time_base: Rational,
}

impl Pipeline {
    fn new(
        width: u32,
        height: u32,
        pix_fmt: Pixel,
        time_base: Rational,
        size: u32,
        fps: u32,
        premultiplied: bool,
    ) -> Result<Self> {
        let mut graph = filter::Graph::new();

        let av_fmt: ffi::AVPixelFormat = pix_fmt.into();
        let args = format!(
            "video_size={width}x{height}:pix_fmt={}:time_base={}/{}:pixel_aspect=1/1",
            av_fmt as i32,
            time_base.numerator(),
            time_base.denominator()
        );
        graph
            .add(
                &filter::find("buffer").context("没有 buffer 滤镜")?,
                "in",
                &args,
            )
            .context("创建滤镜输入失败")?;
        graph
            .add(
                &filter::find("buffersink").context("没有 buffersink 滤镜")?,
                "out",
                "",
            )
            .context("创建滤镜输出失败")?;

        graph
            .output("in", 0)
            .and_then(|p| p.input("out", 0))
            .and_then(|p| p.parse(&filter_spec(size, fps, premultiplied)))
            .map_err(|e| anyhow!("解析滤镜链失败：{e}"))?;
        graph.validate().context("滤镜链校验失败")?;

        Ok(Self {
            graph,
            out: None,
            fps,
            last_pts: None,
            prev_pts: None,
        })
    }

    fn push(&mut self, frame: &mut frame::Video) -> Result<()> {
        if let Some(pts) = frame.pts() {
            self.prev_pts = self.last_pts;
            self.last_pts = Some(pts);
        }
        self.graph
            .get("in")
            .context("找不到滤镜输入")?
            .source()
            .add(frame)
            .context("喂帧给滤镜失败")?;
        // palettegen 要看完所有帧才出调色板，这里通常拿不到东西，属正常
        self.drain()
    }

    /// 把滤镜已经能吐出来的帧编码掉。
    fn drain(&mut self) -> Result<()> {
        loop {
            // 每轮都得用一个干净的帧：av_buffersink_get_frame 内部是 av_frame_move_ref，
            // 不会先 unref 目标帧，复用同一个的话上一帧的缓冲就漏了。
            let mut filtered = frame::Video::empty();
            if self
                .graph
                .get("out")
                .context("找不到滤镜输出")?
                .sink()
                .frame(&mut filtered)
                .is_err()
            {
                return Ok(());
            }
            self.encode(&filtered)?;
        }
    }

    fn encode(&mut self, frame: &frame::Video) -> Result<()> {
        if self.out.is_none() {
            // 到这才知道 paletteuse 输出的真实尺寸和像素格式
            self.out = Some(Encoded::new(
                frame.width(),
                frame.height(),
                frame.format(),
                self.fps,
            )?);
        }
        let out = self.out.as_mut().expect("上面刚建好");
        out.encoder.send_frame(frame).context("送帧给编码器失败")?;
        out.write_packets()
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        // 通知滤镜没有更多输入了，palettegen 这时才会吐调色板。
        // 用 close(pts) 而不是 flush()：把 EOF 的时间点告诉 fps 滤镜，
        // 否则它不知道末帧该持续多久，会把最后一帧丢掉。
        {
            let mut input = self.graph.get("in").context("找不到滤镜输入")?;
            let mut source = input.source();
            match self.last_pts {
                Some(pts) => {
                    // pts 的单位是输入流的 time_base，不同来源差很多：
                    // 裸帧是 1/fps（间隔为 1），webm 常见 1/1000（间隔几十）。
                    // 写死 +1 会把解码来的末帧显示时长砍没。
                    let step = self.prev_pts.map_or(1, |prev| (pts - prev).max(1));
                    source.close(pts + step).context("滤镜收尾失败")?;
                }
                None => source.flush().context("滤镜收尾失败")?,
            }
        }
        self.drain()?;

        let mut out = self.out.take().context("滤镜一帧都没输出")?;
        out.encoder.send_eof().context("通知编码器收尾失败")?;
        out.write_packets()?;
        out.finish()
    }
}

impl Encoded {
    fn new(width: u32, height: u32, pix_fmt: Pixel, fps: u32) -> Result<Self> {
        let codec = encoder::find(codec::Id::GIF).context("没有 GIF 编码器")?;
        let time_base = Rational::new(1, fps.max(1) as i32);

        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .context("创建 GIF 编码器失败")?;
        enc.set_width(width);
        enc.set_height(height);
        enc.set_format(pix_fmt);
        enc.set_time_base(time_base);
        let encoder = enc.open_as(codec).context("打开 GIF 编码器失败")?;

        let (octx, dyn_buf) = memio::open_output("gif")?;
        let mut me = Self {
            encoder,
            octx,
            dyn_buf,
            time_base,
        };

        let mut stream = me.octx.add_stream(codec).context("给 GIF 加输出流失败")?;
        stream.set_parameters(&me.encoder);
        stream.set_time_base(time_base);

        me.octx.write_header().context("写 GIF 文件头失败")?;
        Ok(me)
    }

    fn write_packets(&mut self) -> Result<()> {
        let out_tb = self.octx.stream(0).context("输出流不见了")?.time_base();
        let mut packet = ffmpeg_next::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {}
                Err(ffmpeg_next::Error::Other { errno }) if errno == EAGAIN => return Ok(()),
                Err(ffmpeg_next::Error::Eof) => return Ok(()),
                Err(e) => return Err(anyhow!("取编码结果失败：{e}")),
            }
            packet.set_stream(0);
            packet.rescale_ts(self.time_base, out_tb);
            packet
                .write_interleaved(&mut self.octx)
                .context("写 GIF 帧失败")?;
        }
    }

    fn finish(&mut self) -> Result<Vec<u8>> {
        self.octx.write_trailer().context("写 GIF 文件尾失败")?;
        let bytes = self.take_buffer();
        if bytes.is_empty() {
            bail!("GIF 输出是空的");
        }
        Ok(bytes)
    }

    /// 收走 dyn_buf 里攒的数据，并把 `pb` 和 `dyn_buf` 都置空。
    ///
    /// 两件事必须一起做：
    /// - dyn_buf 只能用 `avio_close_dyn_buf` 关，用 `avio_close` 关会崩；
    /// - 而 ffmpeg-next 的 `Output::drop` 恰恰会无条件 `avio_close(pb)`。
    ///
    /// 所以关完必须把 `pb` 置空（`avio_close(NULL)` 是安全的 no-op）。
    fn take_buffer(&mut self) -> Vec<u8> {
        if self.dyn_buf.is_null() {
            return Vec::new();
        }
        unsafe {
            let mut buf: *mut u8 = ptr::null_mut();
            let len = ffi::avio_close_dyn_buf(self.dyn_buf, &mut buf);
            self.dyn_buf = ptr::null_mut();
            (*self.octx.as_mut_ptr()).pb = ptr::null_mut();
            if len <= 0 || buf.is_null() {
                if !buf.is_null() {
                    ffi::av_free(buf.cast());
                }
                return Vec::new();
            }
            let bytes = slice::from_raw_parts(buf, len as usize).to_vec();
            ffi::av_free(buf.cast());
            bytes
        }
    }
}

impl Drop for Encoded {
    fn drop(&mut self) {
        // 中途出错没走到 finish() 的话，dyn_buf 还挂在 octx 上，
        // 必须在这里按正确的 API 收掉，否则 Output::drop 会崩
        drop(self.take_buffer());
    }
}

/// 抽帧 → 等比缩放（不放大）→ 透明边补成正方形 → 生成调色板并应用。
///
/// 和 PNG 那条路的处理保持一致：不拉变形、不放大、留白透明。
fn filter_spec(size: u32, fps: u32, premultiplied: bool) -> String {
    // rlottie 出来的是预乘 alpha，不还原的话半透明边缘会压暗
    let unpremultiply = if premultiplied {
        "unpremultiply=inplace=1,"
    } else {
        ""
    };
    format!(
        "{unpremultiply}\
         fps={fps},\
         scale='min(iw,{size})':'min(ih,{size})':force_original_aspect_ratio=decrease:flags=lanczos,\
         format=rgba,\
         pad='max(iw,ih)':'max(iw,ih)':(ow-iw)/2:(oh-ih)/2:color=#00000000,\
         split[a][b];\
         [a]palettegen=reserve_transparent=1[p];\
         [b][p]paletteuse=alpha_threshold=128:dither=bayer:bayer_scale=3"
    )
}

/// 给 libavformat 接上内存读写，省掉临时文件。
///
/// 这一小块是整个模块唯一的 unsafe 集中地，其余部分都走 ffmpeg-next 的安全封装。
mod memio {
    use std::{ffi::CString, os::raw::c_void, ptr, slice};

    use anyhow::{Result, bail};
    use ffmpeg_next::{ffi, format::context as fmt_ctx};

    use super::AVIO_BUF;

    /// 内存里的读游标，作为 opaque 指针交给 libav 回调。
    struct Cursor {
        data: Vec<u8>,
        pos: usize,
    }

    /// 持有自定义 AVIO 输入的全部资源，Drop 时按正确顺序释放。
    pub struct Reader {
        ctx: Option<fmt_ctx::Input>,
        avio: *mut ffi::AVIOContext,
        cursor: *mut Cursor,
    }

    unsafe extern "C" fn read(opaque: *mut c_void, buf: *mut u8, size: i32) -> i32 {
        let cursor = unsafe { &mut *opaque.cast::<Cursor>() };
        let left = cursor.data.len().saturating_sub(cursor.pos);
        if left == 0 {
            return ffi::AVERROR_EOF;
        }
        let n = left.min(size.max(0) as usize);
        unsafe { ptr::copy_nonoverlapping(cursor.data[cursor.pos..].as_ptr(), buf, n) };
        cursor.pos += n;
        n as i32
    }

    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    unsafe extern "C" fn seek(opaque: *mut c_void, offset: i64, whence: i32) -> i64 {
        let cursor = unsafe { &mut *opaque.cast::<Cursor>() };
        let len = cursor.data.len() as i64;
        // libav 会用这个标志问「你到底多大」
        if whence == ffi::AVSEEK_SIZE {
            return len;
        }
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => cursor.pos as i64,
            SEEK_END => len,
            _ => return -1,
        };
        let target = base.saturating_add(offset);
        if target < 0 || target > len {
            return -1;
        }
        cursor.pos = target as usize;
        target
    }

    impl Reader {
        pub fn open(data: &[u8]) -> Result<Self> {
            unsafe {
                let cursor = Box::into_raw(Box::new(Cursor {
                    data: data.to_vec(),
                    pos: 0,
                }));

                let buffer = ffi::av_malloc(AVIO_BUF).cast::<u8>();
                if buffer.is_null() {
                    drop(Box::from_raw(cursor));
                    bail!("分配 AVIO 缓冲失败");
                }

                let avio = ffi::avio_alloc_context(
                    buffer,
                    AVIO_BUF as i32,
                    0, // 只读
                    cursor.cast::<c_void>(),
                    Some(read),
                    None,
                    Some(seek),
                );
                if avio.is_null() {
                    ffi::av_free(buffer.cast());
                    drop(Box::from_raw(cursor));
                    bail!("创建 AVIO 上下文失败");
                }

                let mut ptr_ctx = ffi::avformat_alloc_context();
                if ptr_ctx.is_null() {
                    ffi::av_free((*avio).buffer.cast());
                    let mut avio = avio;
                    ffi::avio_context_free(&mut avio);
                    drop(Box::from_raw(cursor));
                    bail!("分配 AVFormatContext 失败");
                }
                (*ptr_ctx).pb = avio;
                // 打上这个标志，avformat_close_input 才不会去动我们自己管的 pb
                (*ptr_ctx).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;

                let mut me = Self {
                    ctx: None,
                    avio,
                    cursor,
                };

                let err = ffi::avformat_open_input(
                    &mut ptr_ctx,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
                if err < 0 {
                    // 失败时 avformat_open_input 已经把 ptr_ctx 释放了
                    bail!("libav 打不开这个文件：{}", av_err(err));
                }
                me.ctx = Some(fmt_ctx::Input::wrap(ptr_ctx));

                let err = ffi::avformat_find_stream_info(ptr_ctx, ptr::null_mut());
                if err < 0 {
                    bail!("读不出流信息：{}", av_err(err));
                }
                Ok(me)
            }
        }

        pub fn context(&mut self) -> &mut fmt_ctx::Input {
            self.ctx.as_mut().expect("open 成功后一定有")
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            unsafe {
                // 先关 AVFormatContext（带 CUSTOM_IO 标志，不会碰 pb），再收我们自己的
                drop(self.ctx.take());
                if !self.avio.is_null() {
                    ffi::av_free((*self.avio).buffer.cast());
                    ffi::avio_context_free(&mut self.avio);
                }
                if !self.cursor.is_null() {
                    drop(Box::from_raw(self.cursor));
                }
            }
        }
    }

    /// 建一个写到内存的输出上下文。
    ///
    /// 返回的 `AVIOContext` 由调用方用 `avio_close_dyn_buf` 收尾，并且**必须**在此之后
    /// 把 `octx.pb` 置空 —— 否则 Output 的 Drop 会再 close 一次。
    pub fn open_output(name: &str) -> Result<(fmt_ctx::Output, *mut ffi::AVIOContext)> {
        unsafe {
            let c_name = CString::new(name).expect("格式名没有 nul");
            let mut ptr_ctx = ptr::null_mut();
            let err = ffi::avformat_alloc_output_context2(
                &mut ptr_ctx,
                ptr::null_mut(),
                c_name.as_ptr(),
                ptr::null(),
            );
            if err < 0 || ptr_ctx.is_null() {
                bail!("创建 {name} 输出上下文失败：{}", av_err(err));
            }

            let mut pb = ptr::null_mut();
            let err = ffi::avio_open_dyn_buf(&mut pb);
            if err < 0 {
                ffi::avformat_free_context(ptr_ctx);
                bail!("创建内存输出缓冲失败：{}", av_err(err));
            }
            (*ptr_ctx).pb = pb;

            Ok((fmt_ctx::Output::wrap(ptr_ctx), pb))
        }
    }

    /// 把 libav 的错误码翻成人能看的字符串。
    fn av_err(code: i32) -> String {
        let mut buf = [0i8; 128];
        unsafe {
            if ffi::av_strerror(code, buf.as_mut_ptr(), buf.len()) < 0 {
                return format!("错误码 {code}");
            }
            let bytes = slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len());
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
    }

    // 这两个类型里的裸指针只在本结构体内部使用，且转换全程在单个阻塞任务里完成
    unsafe impl Send for Reader {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_keeps_aspect_and_transparency() {
        let f = filter_spec(512, 20, false);
        assert!(f.starts_with("fps=20,"));
        // 不放大
        assert!(f.contains("min(iw,512)"));
        // 等比 + 补方
        assert!(f.contains("force_original_aspect_ratio=decrease"));
        assert!(f.contains("pad='max(iw,ih)':'max(iw,ih)'"));
        // 透明保留
        assert!(f.contains("reserve_transparent=1"));
        assert!(f.contains("alpha_threshold=128"));
    }

    #[test]
    fn premultiplied_input_gets_unpremultiplied_first() {
        let f = filter_spec(512, 20, true);
        assert!(
            f.starts_with("unpremultiply=inplace=1,fps=20,"),
            "还原预乘必须排在最前面，实际是 {f}"
        );
        assert!(!filter_spec(512, 20, false).contains("unpremultiply"));
    }

    #[test]
    fn ladders_go_from_large_to_small() {
        for pair in LADDER.windows(2) {
            assert!(pair[0].0 > pair[1].0, "边长应当递减");
            assert!(pair[0].1 >= pair[1].1, "帧率应当递减");
        }
        assert!(PACK_LADDER[0].0 < LADDER[0].0, "整套应当从更小的档位起步");
    }

    #[test]
    fn libav_is_linked_and_has_what_we_need() {
        let ff = probe();
        assert!(ff.available, "GIF 编码器应当可用");
        assert!(ff.libvpx_vp9, "缺 libvpx-vp9 的话视频贴纸会丢透明通道");
    }

    /// 抽帧滤镜链：只取第一帧，等比缩放后补成正方形。
    #[test]
    fn still_filter_takes_first_frame_and_pads() {
        let f = still_filter_spec(256);
        assert!(f.contains("select='eq(n\\,0)'"), "只该取第一帧：{f}");
        // 不放大
        assert!(f.contains("min(iw,256)"));
        assert!(f.contains("force_original_aspect_ratio=decrease"));
        // 补方 + 透明底
        assert!(f.contains("pad='max(iw,ih)':'max(iw,ih)'"));
        assert!(f.contains("color=#00000000"));
        // 抽帧这条链不该带调色板（那是 GIF 才需要的）
        assert!(!f.contains("palettegen"));
        assert!(!f.contains("paletteuse"));
    }

    /// 从裸帧里抽第一帧，产出的是合法 PNG。
    ///
    /// 用一张 64×64 的纯色 BGRA 图，验证整条「滤镜 → PNG 编码 → 内存缓冲」链路。
    #[test]
    fn still_from_raw_frames_produces_a_valid_png() {
        let ff = probe();
        if !ff.available {
            return; // libav 不可用时跳过
        }
        let (w, h) = (64u32, 64u32);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            // 不透明的红，BGRA 顺序
            data.extend_from_slice(&[0, 0, 255, 255]);
        }
        let out = still(
            ff,
            &Input::RawBgra {
                data,
                width: w,
                height: h,
                fps: 60,
            },
            128,
        )
        .expect("抽帧应该成功");

        assert!(!out.bytes.is_empty(), "PNG 不该是空的");
        // PNG magic
        assert_eq!(
            &out.bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        // IHDR 里的宽高：64 的图不会被放大，补方后还是 64×64
        let width = u32::from_be_bytes(out.bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(out.bytes[20..24].try_into().unwrap());
        assert_eq!((width, height), (64, 64));
        // size 报的是实际尺寸，不是请求的 128
        assert_eq!(out.size, 64);
    }

    /// 裸帧数据长度对不上时要报错，不能 panic。
    #[test]
    fn still_rejects_mismatched_raw_frame_data() {
        let ff = probe();
        if !ff.available {
            return;
        }
        let err = still(
            ff,
            &Input::RawBgra {
                data: vec![0u8; 10],
                width: 64,
                height: 64,
                fps: 60,
            },
            128,
        );
        assert!(err.is_err(), "长度对不上应该报错");
    }
}
