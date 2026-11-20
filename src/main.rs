//! 表情包转图标机器人。
//!
//! 给它发贴纸 / 图片，静态的转成方形 PNG，动图转成 GIF，再作为文件发回来；
//! `/pack` 可以一次转换整套表情包，默认打包成 zip。

mod icon;
mod lottie;
mod video;

use std::io::{Cursor, Write};

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, stream};
use mime::Mime;
use teloxide::{
    dispatching::UpdateHandler,
    net::Download,
    prelude::*,
    types::{
        ChatAction, Document, FileId, InputFile, InputMedia, InputMediaDocument, PhotoSize,
        ReplyParameters, Sticker,
    },
    utils::command::BotCommands,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::video::Ffmpeg;

/// Telegram `getFile` 能下载的上限。
const MAX_FILE_SIZE: u32 = 20 * 1024 * 1024;
/// 机器人上传单个文件的上限是 50 MB，留点余量。
const MAX_UPLOAD: usize = 45 * 1024 * 1024;
/// 打包模式一次最多转多少张（一套贴纸最多就是 120 张）。
const MAX_PACK_ZIP: usize = 120;
/// 单张模式一次最多发多少张，发太多会撞 Telegram 的频率限制。
const MAX_PACK_FILES: usize = 30;
/// `sendMediaGroup` 每组最多 10 个文件。
const MEDIA_GROUP: usize = 10;
/// 整套转换时并发下载的数量。
const PACK_CONCURRENCY: usize = 4;

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "支持的命令：")]
enum Command {
    #[command(description = "开始使用")]
    Start,
    #[command(description = "查看用法")]
    Help,
    #[command(description = "转换整套表情包：/pack <名称> [zip|files] [png]")]
    Pack(String),
}

const HELP: &str = "\
🎨 表情包转图标

直接发我一张贴纸、图片或动图，我把它转成方形图标发回来。

格式按素材本身来，不用你指定：
• 静态贴纸 (webp)、照片、图片文件 → PNG
• 视频贴纸 (webm)、动态贴纸 (tgs)、GIF 动图 → GIF，保留透明背景

不管哪种，都会等比缩放并用透明边补成正方形，不拉变形、不放大小图。

整套转换：
/pack <名称> —— 打包成 zip 发回（最多 120 张，静态出 PNG、动图出 GIF）
/pack <名称> png —— 整套只要静态 PNG（动图取缩略图，体积小）
/pack <名称> files —— 一张张单独发（最多 30 张）
也可以回复某张贴纸再发 /pack，名称支持直接粘 t.me/addstickers/xxx 链接

/help —— 再看一遍这段说明";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_logging()?;

    let token = std::env::var("TELOXIDE_TOKEN").context(
        "没读到 TELOXIDE_TOKEN，请在项目根目录的 .env 里写上 TELOXIDE_TOKEN=<你的 token>",
    )?;
    let bot = Bot::new(token);

    let ffmpeg = video::probe();
    match (ffmpeg.available, ffmpeg.libvpx_vp9) {
        (true, true) => log::info!("libav 就绪，动图会转成 GIF"),
        (true, false) => {
            log::warn!("libav 缺少 libvpx-vp9 解码器，视频贴纸的透明背景会丢失")
        }
        (false, _) => log::warn!("libav 起不来，动图会退回成静态缩略图 PNG"),
    }

    let me = bot
        .get_me()
        .await
        .context("连不上 Telegram，检查 token 和网络")?;
    log::info!("已登录 @{}，启动中……", me.username());

    if let Err(e) = bot.set_my_commands(Command::bot_commands()).await {
        log::warn!("设置命令菜单失败（不影响使用）：{e}");
    }

    Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![ffmpeg])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// 默认打到 stdout（跑在 systemd 下就由 journald 收走）；
/// 设了 `LOG_FILE` 就改成追加写那个文件。
fn init_logging() -> Result<()> {
    use pretty_env_logger::env_logger::{Target, WriteStyle};

    let mut builder = pretty_env_logger::formatted_builder();
    builder
        .filter_level(log::LevelFilter::Info)
        .parse_default_env();

    if let Some(path) = std::env::var_os("LOG_FILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打不开日志文件 {}", path.to_string_lossy()))?;
        builder
            .target(Target::Pipe(Box::new(file)))
            .write_style(WriteStyle::Never);
    }

    builder.init();
    Ok(())
}

fn schema() -> UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    Update::filter_message()
        .branch(teloxide::filter_command::<Command, _>().endpoint(on_command))
        .branch(dptree::filter(|msg: Message| has_media(&msg)).endpoint(on_media))
        .branch(dptree::endpoint(on_anything_else))
}

async fn on_command(bot: Bot, msg: Message, cmd: Command, ffmpeg: Ffmpeg) -> HandlerResult {
    match cmd {
        Command::Start | Command::Help => {
            bot.send_message(msg.chat.id, HELP).await?;
        }
        Command::Pack(arg) => {
            if let Err(e) = convert_pack(&bot, &msg, &arg, ffmpeg).await {
                reply_error(&bot, &msg, &e).await?;
            }
        }
    }
    Ok(())
}

async fn on_media(bot: Bot, msg: Message, ffmpeg: Ffmpeg) -> HandlerResult {
    if let Err(e) = convert_one(&bot, &msg, ffmpeg).await {
        reply_error(&bot, &msg, &e).await?;
    }
    Ok(())
}

async fn on_anything_else(bot: Bot, msg: Message) -> HandlerResult {
    // 群里别对无关消息插嘴，只在私聊里提示用法
    if msg.chat.is_private() {
        bot.send_message(msg.chat.id, "发我一张贴纸或图片就行，用法看 /help")
            .reply_parameters(ReplyParameters::new(msg.id))
            .await?;
    }
    Ok(())
}

// ---------- 转换 ----------

/// 一次转换的产物。
struct Converted {
    bytes: Vec<u8>,
    ext: &'static str,
    /// 给用户看的一行说明，例如 `GIF 512×512 @20fps`。
    detail: String,
}

/// 打包时的一个条目。
///
/// 格式（`ext`）跟着数据一起走，不再从文件名反推 —— 文件名里带序号前缀，
/// 靠后缀判断容易在改名、扩展名变化时出错。
#[derive(Clone)]
struct Packed {
    /// zip 里的条目名，例如 `001_sticker_1.png`。
    name: String,
    bytes: Vec<u8>,
    /// 输出格式：静态 `png`，动图 `gif`。
    ext: &'static str,
}

impl Packed {
    fn is_gif(&self) -> bool {
        self.ext == "gif"
    }
}

/// 下载并转换：按 `format` 决定出 PNG 还是 GIF。
///
/// `Format::Auto` 下格式跟着素材走 —— 静态的出 PNG，动图的出 GIF；
/// `Format::Static` 下不管什么素材都出静态 PNG。
async fn convert_media(
    bot: &Bot,
    source: &Source,
    ffmpeg: Ffmpeg,
    format: Format,
    ladder: &[(u32, u32)],
    limit: usize,
) -> Result<Converted> {
    // 最终出什么：整张决策表在 output_of 里。
    let mut output = output_of(format, source.motion, ffmpeg.available);

    if output == Output::Gif {
        match to_gif(bot, source, ffmpeg, ladder, limit).await {
            Ok(gif) => {
                return Ok(Converted {
                    detail: format!("GIF {}×{} @{}fps", gif.size, gif.size, gif.fps),
                    bytes: gif.bytes,
                    ext: "gif",
                });
            }
            // 转不了也别整条失败，退回静态图
            Err(e) => {
                log::warn!("{} 转 GIF 失败，退回静态图：{e:#}", source.stem);
                output = gif_failure_fallback(source.motion);
            }
        }
    }

    // 静态分支。
    //
    // 原文件本身不是静态图片（webm / mp4 / tgs）就得另找一张静态图：
    // 优先用 Telegram 给的缩略图；没有缩略图就让 libav 抽第一帧。
    // （缩略图是服务端行为、没写进文档，不能假设一定有。）
    let still_bytes: Vec<u8> = match (&source.fallback, output) {
        (Some(fallback), _) => download(bot, fallback).await?,
        // 静态素材：直接用原图
        (None, Output::Png) => download(bot, &source.file).await?,
        // 动图降级且没有缩略图：抽第一帧
        (None, Output::PngFromMotion) => {
            if !ffmpeg.available {
                bail!("{} 没有缩略图，libav 不可用，取不到静态帧", source.stem);
            }
            let data = download(bot, &source.file).await?;
            let input = match source.motion {
                Motion::Lottie => {
                    // tgs 先用 rlottie 渲成裸帧
                    let (size, _fps) = ladder.first().copied().context("阶梯是空的")?;
                    let frames =
                        tokio::task::spawn_blocking(move || lottie::render(&data, size, 1))
                            .await
                            .context("渲染任务被中断")??;
                    video::Input::RawBgra {
                        data: frames.data,
                        width: frames.width,
                        height: frames.height,
                        fps: frames.fps,
                    }
                }
                _ => video::Input::Encoded {
                    data,
                    vp9: source.motion == Motion::Vp9,
                },
            };
            let still = video::to_still(ffmpeg, input, icon::MAX_SIZE)
                .await
                .context("抽第一帧失败")?;
            // 抽出来的已经是方形 PNG 了，直接用
            return Ok(Converted {
                detail: format!("PNG {}×{}（动图第一帧）", still.size, still.size),
                bytes: still.bytes,
                ext: "png",
            });
        }
        // 到不了这儿：output == Gif 的分支上面已经 return 了
        (None, Output::Gif) => bail!("内部错误：GIF 分支已经处理过了"),
    };

    let icon = tokio::task::spawn_blocking(move || icon::convert(&still_bytes))
        .await
        .context("转换任务被中断")??;

    Ok(Converted {
        // 尺寸有变化时把原图尺寸也带上，方便看出被缩过
        detail: if icon.source == (icon.size, icon.size) {
            format!("PNG {}×{}", icon.size, icon.size)
        } else {
            format!(
                "PNG {}×{}（原图 {}×{}）",
                icon.size, icon.size, icon.source.0, icon.source.1
            )
        },
        bytes: icon.bytes,
        ext: "png",
    })
}

/// 下载动图并转成 GIF。tgs 要先用 rlottie 渲染成裸帧，其余直接丢给 ffmpeg。
async fn to_gif(
    bot: &Bot,
    source: &Source,
    ffmpeg: Ffmpeg,
    ladder: &[(u32, u32)],
    limit: usize,
) -> Result<video::Gif> {
    let data = download(bot, &source.file).await?;

    if source.motion == Motion::Lottie {
        // 矢量动画，直接按目标尺寸渲染，比先渲大图再缩更清楚
        let (size, fps) = ladder.first().copied().context("阶梯是空的")?;
        let frames = tokio::task::spawn_blocking(move || lottie::render(&data, size, fps))
            .await
            .context("渲染任务被中断")??;
        log::debug!(
            "tgs 渲染出 {} 帧 {}×{} @{}fps",
            frames.count,
            frames.width,
            frames.height,
            frames.fps
        );
        return video::to_gif(
            ffmpeg,
            video::Input::RawBgra {
                data: frames.data,
                width: frames.width,
                height: frames.height,
                fps: frames.fps,
            },
            ladder,
            limit,
        )
        .await;
    }

    video::to_gif(
        ffmpeg,
        video::Input::Encoded {
            data,
            vp9: source.motion == Motion::Vp9,
        },
        ladder,
        limit,
    )
    .await
}

/// 转换单条消息里的素材。
async fn convert_one(bot: &Bot, msg: &Message, ffmpeg: Ffmpeg) -> Result<()> {
    let source = source_of(msg, ffmpeg)?;
    bot.send_chat_action(msg.chat.id, ChatAction::UploadDocument)
        .await
        .ok();

    let out = convert_media(
        bot,
        &source,
        ffmpeg,
        Format::Auto,
        &video::LADDER,
        video::SOFT_LIMIT,
    )
    .await?;

    let mut caption = format!(
        "✅ {}\n→ {}\n大小 {}",
        source.origin,
        out.detail,
        human_size(out.bytes.len())
    );
    if let Some(set) = &source.set_name {
        caption.push_str(&format!("\n所属表情包 {set}，发 /pack {set} 可以转换整套"));
    }

    let file = InputFile::memory(out.bytes).file_name(format!("{}.{}", source.stem, out.ext));
    bot.send_document(msg.chat.id, file)
        .caption(caption)
        // 关掉服务端内容类型探测，否则 Telegram 会把 GIF 转成 MP4 动画，
        // 用户下下来是个 .gif 名字的 mp4
        .disable_content_type_detection(true)
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
    Ok(())
}

/// 整套表情包怎么发回去。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Delivery {
    /// 打包成一个 zip。
    #[default]
    Zip,
    /// 一张张单独发。
    Files,
}

/// 输出的图片格式。
///
/// 格式默认由素材自己决定 —— 静态的出 PNG，动图的出 GIF；
/// 只有明确要求「整套只要静态」时才把动图降级成缩略图。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// 按素材自身类型分流：静态出 PNG，动图出 GIF。
    Auto,
    /// 全部出静态 PNG（动图取缩略图当静态图处理）。
    Static,
}

/// 这次转换最终出什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    /// 静态 PNG。
    Png,
    /// 动图降级成的静态 PNG（取缩略图，没有就抽第一帧）。
    PngFromMotion,
    /// GIF。
    Gif,
}

/// 这份素材最终出什么格式。
///
/// 整张决策表都在这儿，纯函数，不碰网络，方便直接把表测一遍：
///
/// | 格式策略 | 素材 | libav | 结果 |
/// | --- | --- | --- | --- |
/// | Auto | 静态 | 有无 | PNG |
/// | Auto | 动图 | 有 | GIF |
/// | Auto | 动图 | 没有 | 静态 PNG（降级） |
/// | Static | 静态 | 有无 | PNG |
/// | Static | 动图 | 有无 | 静态 PNG（降级） |
fn output_of(format: Format, motion: Motion, ffmpeg_available: bool) -> Output {
    match (format, motion) {
        // 强制静态：不管什么素材都出静态 PNG，动图降级。
        (Format::Static, Motion::None) => Output::Png,
        (Format::Static, _) => Output::PngFromMotion,
        // 按类型分流：静态出 PNG。
        (Format::Auto, Motion::None) => Output::Png,
        // 动图出 GIF —— 但 libav 起不来就只剩静态图可出。
        (Format::Auto, _) if ffmpeg_available => Output::Gif,
        (Format::Auto, _) => Output::PngFromMotion,
    }
}

/// GIF 编码失败时退到哪。
///
/// 出不了 GIF 就退回静态图：静态素材用原图，动图用缩略图或第一帧。
fn gif_failure_fallback(motion: Motion) -> Output {
    match motion {
        Motion::None => Output::Png,
        _ => Output::PngFromMotion,
    }
}

/// 给用户看的格式说明。
fn format_label(format: Format) -> &'static str {
    match format {
        Format::Auto => "静态 PNG、动图 GIF",
        Format::Static => "静态 PNG",
    }
}

/// `/pack` 的参数。
struct PackArgs {
    name: Option<String>,
    delivery: Delivery,
    /// 输出格式，默认按素材自身类型分流。
    format: Format,
}

fn parse_pack_args(arg: &str) -> PackArgs {
    let mut name = None;
    let mut delivery = Delivery::default();
    let mut format = Format::Auto;

    for token in arg.split_whitespace() {
        match token.to_ascii_lowercase().as_str() {
            "zip" | "打包" => delivery = Delivery::Zip,
            "files" | "file" | "单张" => delivery = Delivery::Files,
            // gif 现在就是默认行为（按类型分流），留着这个关键字只是为了兼容旧命令。
            // 它不改变格式，所以跟 png 同时出现时由 png 说了算 —— 关键字的优先级跟顺序无关。
            "gif" | "动图" => {}
            "png" | "静态" => format = Format::Static,
            _ if name.is_none() => name = pack_name(token),
            _ => {}
        }
    }

    PackArgs {
        name,
        delivery,
        format,
    }
}

/// 转换整套表情包。
async fn convert_pack(bot: &Bot, msg: &Message, arg: &str, ffmpeg: Ffmpeg) -> Result<()> {
    let args = parse_pack_args(arg);
    let set_name = args
        .name
        .or_else(|| msg.reply_to_message()?.sticker()?.set_name.clone())
        .ok_or_else(|| {
            anyhow!(
                "要指定表情包：/pack <名称> [zip|files] [png]，或者回复一张贴纸再发 /pack\n\
                 名称就是分享链接 t.me/addstickers/ 后面那段"
            )
        })?;

    let set = bot
        .get_sticker_set(set_name.clone())
        .await
        .with_context(|| format!("找不到表情包 {set_name}"))?;

    let total = set.stickers.len();
    if total == 0 {
        bail!("表情包 {set_name} 是空的");
    }

    let cap = match args.delivery {
        Delivery::Zip => MAX_PACK_ZIP,
        Delivery::Files => MAX_PACK_FILES,
    };
    let picked: Vec<Sticker> = set.stickers.into_iter().take(cap).collect();
    let over_cap = total - picked.len();

    bot.send_message(
        msg.chat.id,
        format!(
            "开始转换《{}》，共 {} 张，正在处理 {} 张（{}）……",
            set.title,
            total,
            picked.len(),
            format_label(args.format)
        ),
    )
    .reply_parameters(ReplyParameters::new(msg.id))
    .await?;
    bot.send_chat_action(msg.chat.id, ChatAction::UploadDocument)
        .await
        .ok();

    // 有限并发地下载 + 转换，结果保持原顺序
    let converted: Vec<Result<Packed>> = stream::iter(picked.into_iter().enumerate())
        .map(|(i, sticker)| async move {
            let source =
                sticker_source(&sticker, ffmpeg).with_context(|| format!("第 {} 张失败", i + 1))?;
            let out = convert_media(
                bot,
                &source,
                ffmpeg,
                args.format,
                &video::PACK_LADDER,
                video::PACK_SOFT_LIMIT,
            )
            .await
            .with_context(|| format!("第 {} 张失败", i + 1))?;
            // 加序号前缀，解压出来能保持表情包原本的顺序
            Ok(Packed {
                name: format!("{:03}_{}.{}", i + 1, source.stem, out.ext),
                bytes: out.bytes,
                ext: out.ext,
            })
        })
        .buffered(PACK_CONCURRENCY)
        .collect()
        .await;

    let mut files = Vec::new();
    let mut failures = Vec::new();
    for result in converted {
        match result {
            Ok(item) => files.push(item),
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    if files.is_empty() {
        bail!("整套都没转成功：{}", failures.join("；"));
    }

    let converted_count = files.len();
    // 混合格式下 GIF 是体积大头，超预算时提示用户改走纯静态
    let has_gif = files.iter().any(|f| f.is_gif());
    let over_budget = match args.delivery {
        Delivery::Zip => send_as_zip(bot, msg, &set_name, &set.title, files).await?,
        Delivery::Files => {
            send_as_files(bot, msg, files).await?;
            0
        }
    };

    let sent = converted_count - over_budget;
    let summary = build_summary(&PackSummary {
        title: &set.title,
        sent,
        total,
        cap,
        over_cap,
        over_budget,
        has_gif,
        failures: &failures,
    });
    bot.send_message(msg.chat.id, summary).await?;
    Ok(())
}

/// 一套转换的统计结果，用来拼最后那条总结。
struct PackSummary<'a> {
    title: &'a str,
    /// 实际发回去的张数。
    sent: usize,
    /// 表情包总张数。
    total: usize,
    /// 单次处理上限。
    cap: usize,
    /// 超过上限、这次没转的张数。
    over_cap: usize,
    /// 转了但装不下、被丢掉的张数。
    over_budget: usize,
    /// 这一套里有没有 GIF。
    has_gif: bool,
    /// 每张失败的原因。
    failures: &'a [String],
}

/// 拼最后那条总结。
///
/// 纯函数，不碰网络 —— 文案要能被直接测。
fn build_summary(s: &PackSummary) -> String {
    let mut summary = format!("✅《{}》完成，发回 {}/{} 张", s.title, s.sent, s.total);
    if s.over_cap > 0 {
        summary.push_str(&format!(
            "\n⚠️ 单次上限 {} 张，剩下 {} 张没转",
            s.cap, s.over_cap
        ));
    }
    if s.over_budget > 0 {
        summary.push_str(&format!(
            "\n⚠️ 超过 {} 的上传上限，最后 {} 张没放进压缩包",
            human_size(MAX_UPLOAD),
            s.over_budget
        ));
        if s.has_gif {
            summary.push_str("\n💡 GIF 体积大，加 png 参数改成纯静态 PNG 能装下整套");
        }
    }
    if !s.failures.is_empty() {
        summary.push_str(&format!(
            "\n⚠️ {} 张失败：{}",
            s.failures.len(),
            s.failures.join("；")
        ));
    }
    summary
}

/// 打成 zip 发回去，返回因为超出上传上限被丢掉的张数。
async fn send_as_zip(
    bot: &Bot,
    msg: &Message,
    set_name: &str,
    title: &str,
    mut files: Vec<Packed>,
) -> Result<usize> {
    let dropped = fit_upload_budget(&mut files);
    let count = files.len();

    let archive = tokio::task::spawn_blocking(move || build_zip(&files))
        .await
        .context("打包任务被中断")??;

    let name = format!("{}.zip", sanitize(set_name));
    let caption = format!(
        "📦《{title}》{count} 张\n压缩包 {}",
        human_size(archive.len())
    );
    bot.send_document(msg.chat.id, InputFile::memory(archive).file_name(name))
        .caption(caption)
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
    Ok(dropped)
}

/// 一张张发回去，10 个一组。
///
/// 媒体组里的文档，Telegram 强制不做内容类型探测，所以 GIF 不会被转成 MP4；
/// 落单那张走 sendDocument，得自己关掉。
async fn send_as_files(bot: &Bot, msg: &Message, files: Vec<Packed>) -> Result<()> {
    for chunk in files.chunks(MEDIA_GROUP) {
        // sendMediaGroup 要求 2~10 个条目，落单的那张走 sendDocument
        if let [one] = chunk {
            let file = InputFile::memory(one.bytes.clone()).file_name(one.name.clone());
            bot.send_document(msg.chat.id, file)
                .disable_content_type_detection(true)
                .await?;
            continue;
        }
        let group: Vec<InputMedia> = chunk
            .iter()
            .map(|item| {
                let file = InputFile::memory(item.bytes.clone()).file_name(item.name.clone());
                InputMedia::Document(InputMediaDocument::new(file))
            })
            .collect();
        bot.send_media_group(msg.chat.id, group).await?;
    }
    Ok(())
}

/// 全程在内存里打包，不落任何临时文件。
fn build_zip(files: &[Packed]) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    // PNG / GIF 本身已经压过了，deflate 基本只是走个过场，但保持成通用格式
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for item in files {
        writer
            .start_file(item.name.as_str(), options)
            .and_then(|()| writer.write_all(&item.bytes).map_err(Into::into))
            .with_context(|| format!("往 zip 里写 {} 失败", item.name))?;
    }

    Ok(writer.finish().context("生成 zip 失败")?.into_inner())
}

/// 按上传上限裁掉装不下的部分，返回被丢掉的张数。
///
/// 混合格式下（一套里静态 PNG 和动图 GIF 都有），GIF 是体积大头。
/// 装不下时**优先丢 GIF**：静态的那部分体积小得多，而且单独再发一次就能补回来；
/// 按顺序截断则会把尾部一整段砍掉，混合包里那一段可能正好全是用户想要的素材。
fn fit_upload_budget(files: &mut Vec<Packed>) -> usize {
    // 每个条目除了数据本身还有本地头 + 中央目录，估 128 字节余量
    const ENTRY_OVERHEAD: usize = 128;

    fn cost_of(item: &Packed) -> usize {
        item.bytes.len() + item.name.len() * 2 + ENTRY_OVERHEAD
    }

    // 先算所有条目的开销，总体积超了就从最大的 GIF 开始丢。
    let mut total: usize = files.iter().map(cost_of).sum();
    if total <= MAX_UPLOAD {
        return 0;
    }

    // 按体积从大到小丢掉 GIF，直到装得下。
    let mut gif_order: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_gif())
        .map(|(i, _)| i)
        .collect();
    gif_order.sort_by_key(|&i| std::cmp::Reverse(files[i].bytes.len()));

    let mut dropped_set = std::collections::HashSet::new();
    for &i in &gif_order {
        if total <= MAX_UPLOAD {
            break;
        }
        total -= cost_of(&files[i]);
        dropped_set.insert(i);
    }

    // 全丢完 GIF 还超限（静态 PNG 本身就很多），那就再按体积从大到小丢静态。
    if total > MAX_UPLOAD {
        let mut rest: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(i, _)| !dropped_set.contains(i))
            .map(|(i, _)| i)
            .collect();
        rest.sort_by_key(|&i| std::cmp::Reverse(files[i].bytes.len()));
        for i in rest {
            if total <= MAX_UPLOAD {
                break;
            }
            total -= cost_of(&files[i]);
            dropped_set.insert(i);
        }
    }

    // 保留下来的按原顺序输出。序号前缀是原表情包里的位置，
    // 所以中间被丢的那几张会留成缺口 —— 缺口正好告诉用户少了哪几张。
    let mut kept = Vec::with_capacity(files.len() - dropped_set.len());
    for (i, item) in files.iter().enumerate() {
        if !dropped_set.contains(&i) {
            kept.push(item.clone());
        }
    }
    let dropped = files.len() - kept.len();
    *files = kept;
    dropped
}
/// 把 Telegram 上的文件下到内存里。
async fn download(bot: &Bot, file_id: &FileId) -> Result<Vec<u8>> {
    let file = bot
        .get_file(file_id.clone())
        .await
        .context("获取文件信息失败")?;
    if file.size > MAX_FILE_SIZE {
        bail!(
            "文件 {} 太大了，上限 {}",
            human_size(file.size as usize),
            human_size(MAX_FILE_SIZE as usize)
        );
    }

    let mut data = Vec::with_capacity(file.size as usize);
    bot.download_file(&file.path, &mut data)
        .await
        .context("下载文件失败")?;
    Ok(data)
}

// ---------- 从消息里找素材 ----------

/// 动图素材属于哪一类，决定交给谁去解。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    /// 不是动图，走静态 PNG。
    None,
    /// 已编码的容器（gif / mp4），ffmpeg 自己 demux。
    Encoded,
    /// VP9 webm 视频贴纸，要显式指定 libvpx-vp9 才能保住 alpha。
    Vp9,
    /// tgs 矢量动画，先用 rlottie 渲染成裸帧。
    Lottie,
}

/// 一条消息里可以拿来做图标的素材。
struct Source {
    /// 首选文件：动图就是动图本体，静态就是图片本身。
    file: FileId,
    /// `file` 不是静态图片时（webm / mp4 / tgs），退回用的静态缩略图。
    fallback: Option<FileId>,
    /// 动图类型。
    motion: Motion,
    /// 输出文件名的主干（不含扩展名）。
    stem: String,
    /// 给用户看的来源说明。
    origin: String,
    /// 贴纸所属表情包，用来提示 `/pack`。
    set_name: Option<String>,
}

impl Source {
    /// 静态图片素材。
    fn still(file: FileId, stem: String, origin: String) -> Self {
        Self {
            file,
            fallback: None,
            motion: Motion::None,
            stem,
            origin,
            set_name: None,
        }
    }
}

fn has_media(msg: &Message) -> bool {
    msg.sticker().is_some()
        || msg.photo().is_some()
        || msg.animation().is_some()
        || msg.document().is_some_and(is_image_document)
}

/// 图片文件的 MIME 类型算不算动图。
///
/// 动画 GIF 交给 ffmpeg 解；别的图片文件按静态处理。
/// 纯函数，方便直接测。
fn motion_of_mime(mime: Option<&Mime>) -> Motion {
    match mime {
        Some(m) if m.subtype() == "gif" => Motion::Encoded,
        _ => Motion::None,
    }
}

fn source_of(msg: &Message, ffmpeg: Ffmpeg) -> Result<Source> {
    if let Some(sticker) = msg.sticker() {
        return sticker_source(sticker, ffmpeg);
    }
    if let Some(animation) = msg.animation() {
        // Telegram 里的「GIF」实际是 mp4
        return Ok(Source {
            file: animation.file.id.clone(),
            fallback: animation.thumbnail.as_ref().map(|t| t.file.id.clone()),
            motion: Motion::Encoded,
            stem: format!("gif_{}", sanitize(&animation.file.unique_id.0)),
            origin: "动图".to_string(),
            set_name: None,
        });
    }
    if let Some(photos) = msg.photo() {
        let largest = largest_photo(photos).context("这条消息里没有可用的图片尺寸")?;
        return Ok(Source::still(
            largest.file.id.clone(),
            format!("photo_{}", sanitize(&largest.file.unique_id.0)),
            "照片".to_string(),
        ));
    }
    if let Some(doc) = msg.document().filter(|d| is_image_document(d)) {
        let stem = doc
            .file_name
            .as_deref()
            .map(file_stem)
            .map(sanitize)
            .unwrap_or_else(|| format!("image_{}", sanitize(&doc.file.unique_id.0)));
        let mime = doc.mime_type.as_ref();
        let origin = mime.map_or_else(|| "图片文件".to_string(), |m| format!("图片文件 ({m})"));
        // 动画 GIF 交给 ffmpeg 抽帧；解不开就退回自己的第一帧
        let motion = motion_of_mime(mime);
        return Ok(Source {
            file: doc.file.id.clone(),
            fallback: None,
            motion,
            stem,
            origin,
            set_name: None,
        });
    }
    bail!("这条消息里没有能转换的图片")
}

fn sticker_source(sticker: &Sticker, ffmpeg: Ffmpeg) -> Result<Source> {
    let thumbnail = sticker.thumbnail.as_ref().map(|t| t.file.id.clone());
    let stem = match &sticker.set_name {
        Some(set) => format!("{}_{}", sanitize(set), sanitize(&sticker.file.unique_id.0)),
        None => format!("sticker_{}", sanitize(&sticker.file.unique_id.0)),
    };

    let mut source = if sticker.is_static() {
        Source::still(sticker.file.id.clone(), stem, "静态贴纸 (webp)".to_string())
    } else {
        // webm 和 tgs 都能出 GIF，只是解法不同；ffmpeg 缺席就退回缩略图
        let (motion, label) = if sticker.is_video() {
            (Motion::Vp9, "视频贴纸 (webm)")
        } else {
            (Motion::Lottie, "动态贴纸 (tgs)")
        };
        if !ffmpeg.available && thumbnail.is_none() {
            bail!("{label} 要靠 libav 才能转，当前起不来，这张也没有缩略图");
        }
        Source {
            file: sticker.file.id.clone(),
            fallback: thumbnail,
            motion,
            stem,
            origin: if ffmpeg.available {
                label.to_string()
            } else {
                format!("{label} 的缩略图（libav 不可用）")
            },
            set_name: None,
        }
    };

    source.set_name = sticker.set_name.clone();
    Ok(source)
}

fn largest_photo(photos: &[PhotoSize]) -> Option<&PhotoSize> {
    photos
        .iter()
        .max_by_key(|p| u64::from(p.width) * u64::from(p.height))
}

fn is_image_document(doc: &Document) -> bool {
    doc.mime_type.as_ref().is_some_and(|m| m.type_() == "image")
}

// ---------- 杂项 ----------

async fn reply_error(
    bot: &Bot,
    msg: &Message,
    err: &anyhow::Error,
) -> Result<(), teloxide::RequestError> {
    log::warn!("chat {}: {err:#}", msg.chat.id);
    bot.send_message(msg.chat.id, format!("❌ {err:#}"))
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
    Ok(())
}

/// 去掉扩展名。
fn file_stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// 只留下文件名里安全的 ASCII 字符，避免中文 / 表情符号在下载时出问题。
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "icon".to_string()
    } else {
        trimmed.chars().take(48).collect()
    }
}

/// 从 `/pack` 的参数里取表情包名，顺便支持直接粘分享链接。
fn pack_name(arg: &str) -> Option<String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return None;
    }
    let name = arg
        .rsplit_once("addstickers/")
        .map_or(arg, |(_, name)| name)
        .trim_matches('/')
        .split(['?', '&'])
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then(|| name.to_string())
}

fn human_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes:.0} B")
    } else if bytes < KIB * KIB {
        format!("{:.1} KB", bytes / KIB)
    } else {
        format!("{:.1} MB", bytes / KIB / KIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_unsafe_characters() {
        assert_eq!(sanitize("hello_world-1"), "hello_world-1");
        assert_eq!(sanitize("表情包"), "icon");
        assert_eq!(sanitize("a b/c.d"), "a_b_c_d");
        assert_eq!(sanitize("__"), "icon");
        assert_eq!(sanitize(&"x".repeat(80)).len(), 48);
    }

    #[test]
    fn file_stem_drops_extension() {
        assert_eq!(file_stem("cat.png"), "cat");
        assert_eq!(file_stem("cat.tar.gz"), "cat.tar");
        assert_eq!(file_stem("noext"), "noext");
    }

    #[test]
    fn pack_name_accepts_plain_names_and_links() {
        assert_eq!(pack_name("  MyPack "), Some("MyPack".to_string()));
        assert_eq!(
            pack_name("https://t.me/addstickers/MyPack"),
            Some("MyPack".to_string())
        );
        assert_eq!(
            pack_name("t.me/addstickers/MyPack/"),
            Some("MyPack".to_string())
        );
        assert_eq!(
            pack_name("https://t.me/addstickers/MyPack?single"),
            Some("MyPack".to_string())
        );
        assert_eq!(pack_name("   "), None);
    }

    #[test]
    fn pack_args_default_to_auto_format() {
        let args = parse_pack_args("MyPack");
        assert_eq!(args.name.as_deref(), Some("MyPack"));
        assert_eq!(args.delivery, Delivery::Zip);
        assert_eq!(args.format, Format::Auto, "整套默认按类型自动分流");
    }

    #[test]
    fn pack_args_read_keywords_in_any_order() {
        let args = parse_pack_args("MyPack files gif");
        assert_eq!(args.name.as_deref(), Some("MyPack"));
        assert_eq!(args.delivery, Delivery::Files);
        assert_eq!(args.format, Format::Auto);

        let args = parse_pack_args("  GIF   MyPack  ZIP ");
        assert_eq!(args.name.as_deref(), Some("MyPack"));
        assert_eq!(args.delivery, Delivery::Zip);
        assert_eq!(args.format, Format::Auto);
    }

    /// `gif` 现在只是兼容用的关键字 —— 它不改格式，跟顺序无关。
    #[test]
    fn pack_args_gif_keyword_is_a_no_op() {
        let args = parse_pack_args("MyPack gif");
        assert_eq!(args.name.as_deref(), Some("MyPack"));
        assert_eq!(args.format, Format::Auto);

        let args = parse_pack_args("MyPack 动图");
        assert_eq!(args.format, Format::Auto);
    }

    /// `png` 是明确要求「整套只要静态」，压过兼容关键字，跟顺序无关。
    #[test]
    fn pack_args_png_wins_over_the_gif_keyword() {
        let args = parse_pack_args("MyPack png gif");
        assert_eq!(args.format, Format::Static);

        let args = parse_pack_args("MyPack gif png");
        assert_eq!(args.format, Format::Static);
    }

    /// 想要整套纯静态时显式说 png。
    #[test]
    fn pack_args_png_forces_static() {
        let args = parse_pack_args("MyPack png");
        assert_eq!(args.format, Format::Static);

        let args = parse_pack_args("MyPack 静态");
        assert_eq!(args.format, Format::Static);
    }

    /// 整张决策表：格式策略 × 素材类型 × libav 在不在。
    #[test]
    fn output_table_covers_every_combination() {
        // 按类型自动分流：静态出 PNG，动图出 GIF。
        assert_eq!(output_of(Format::Auto, Motion::None, true), Output::Png);
        assert_eq!(output_of(Format::Auto, Motion::Encoded, true), Output::Gif);
        assert_eq!(output_of(Format::Auto, Motion::Vp9, true), Output::Gif);
        assert_eq!(output_of(Format::Auto, Motion::Lottie, true), Output::Gif);

        // 强制静态：一律 PNG，动图降级。
        assert_eq!(output_of(Format::Static, Motion::None, true), Output::Png);
        assert_eq!(
            output_of(Format::Static, Motion::Encoded, true),
            Output::PngFromMotion
        );
        assert_eq!(
            output_of(Format::Static, Motion::Vp9, true),
            Output::PngFromMotion
        );
        assert_eq!(
            output_of(Format::Static, Motion::Lottie, true),
            Output::PngFromMotion
        );

        // libav 起不来：动图只能降级成静态 PNG，静态的还是静态 PNG。
        assert_eq!(output_of(Format::Auto, Motion::None, false), Output::Png);
        assert_eq!(
            output_of(Format::Auto, Motion::Encoded, false),
            Output::PngFromMotion
        );
        assert_eq!(
            output_of(Format::Auto, Motion::Vp9, false),
            Output::PngFromMotion
        );
        assert_eq!(
            output_of(Format::Auto, Motion::Lottie, false),
            Output::PngFromMotion
        );
        assert_eq!(
            output_of(Format::Static, Motion::Encoded, false),
            Output::PngFromMotion
        );
    }

    /// GIF 编码失败时的兜底：退回静态图。
    #[test]
    fn gif_failure_falls_back_to_a_still() {
        assert_eq!(gif_failure_fallback(Motion::None), Output::Png);
        assert_eq!(gif_failure_fallback(Motion::Encoded), Output::PngFromMotion);
        assert_eq!(gif_failure_fallback(Motion::Vp9), Output::PngFromMotion);
        assert_eq!(gif_failure_fallback(Motion::Lottie), Output::PngFromMotion);
    }

    /// 图片文件的 MIME 决定算不算动图。
    #[test]
    fn mime_decides_whether_a_document_is_motion() {
        let gif: Mime = "image/gif".parse().unwrap();
        let png: Mime = "image/png".parse().unwrap();
        let jpeg: Mime = "image/jpeg".parse().unwrap();

        assert_eq!(motion_of_mime(Some(&gif)), Motion::Encoded);
        assert_eq!(motion_of_mime(Some(&png)), Motion::None);
        assert_eq!(motion_of_mime(Some(&jpeg)), Motion::None);
        assert_eq!(motion_of_mime(None), Motion::None);
    }

    /// 格式说明给用户看。
    #[test]
    fn format_label_describes_the_mode() {
        assert_eq!(format_label(Format::Auto), "静态 PNG、动图 GIF");
        assert_eq!(format_label(Format::Static), "静态 PNG");
    }

    /// 总结文案：正常情况、超上限、超预算、失败分别怎么写。
    #[test]
    fn summary_reports_what_happened() {
        let plain = build_summary(&PackSummary {
            title: "MyPack",
            sent: 10,
            total: 10,
            cap: 120,
            over_cap: 0,
            over_budget: 0,
            has_gif: false,
            failures: &[],
        });
        assert_eq!(plain, "✅《MyPack》完成，发回 10/10 张");
        assert!(!plain.contains("⚠️"), "一切正常不该有警告：{plain}");

        let capped = build_summary(&PackSummary {
            title: "MyPack",
            sent: 120,
            total: 200,
            cap: 120,
            over_cap: 80,
            over_budget: 0,
            has_gif: false,
            failures: &[],
        });
        assert!(
            capped.contains("单次上限 120 张，剩下 80 张没转"),
            "{capped}"
        );

        let over_budget = build_summary(&PackSummary {
            title: "MyPack",
            sent: 95,
            total: 100,
            cap: 120,
            over_cap: 0,
            over_budget: 5,
            has_gif: true,
            failures: &[],
        });
        assert!(
            over_budget.contains("最后 5 张没放进压缩包"),
            "{over_budget}"
        );
        assert!(over_budget.contains("加 png 参数"), "{over_budget}");

        // 没有 GIF 就不给「加 png」这条建议（已经全是静态了）
        let static_over_budget = build_summary(&PackSummary {
            title: "MyPack",
            sent: 95,
            total: 100,
            cap: 120,
            over_cap: 0,
            over_budget: 5,
            has_gif: false,
            failures: &[],
        });
        assert!(
            !static_over_budget.contains("加 png 参数"),
            "{static_over_budget}"
        );

        let failed = build_summary(&PackSummary {
            title: "MyPack",
            sent: 9,
            total: 10,
            cap: 120,
            over_cap: 0,
            over_budget: 0,
            has_gif: false,
            failures: &["第 3 张失败".to_string()],
        });
        assert!(failed.contains("1 张失败：第 3 张失败"), "{failed}");
    }

    /// 回复贴纸时只给方式不给名字，名字得留空好从被回复的消息里取
    #[test]
    fn pack_args_keyword_is_not_mistaken_for_a_name() {
        let args = parse_pack_args("files");
        assert_eq!(args.name, None);
        assert_eq!(args.delivery, Delivery::Files);

        let args = parse_pack_args("");
        assert_eq!(args.name, None);
        assert_eq!(args.delivery, Delivery::Zip);
    }

    /// 造一个打包条目。
    fn packed(name: &str, ext: &str, bytes: Vec<u8>) -> Packed {
        Packed {
            name: name.to_string(),
            bytes,
            ext: match ext {
                "gif" => "gif",
                _ => "png",
            },
        }
    }

    #[test]
    fn zip_contains_every_file() {
        let files = vec![
            packed("001_a.png", "png", vec![1u8; 100]),
            packed("002_b.gif", "gif", vec![2u8; 200]),
        ];
        let archive = build_zip(&files).unwrap();

        let mut reader = zip::ZipArchive::new(Cursor::new(archive)).unwrap();
        assert_eq!(reader.len(), 2);
        assert_eq!(reader.by_index(0).unwrap().name(), "001_a.png");
        assert_eq!(reader.by_index(1).unwrap().name(), "002_b.gif");
    }

    #[test]
    fn upload_budget_drops_what_does_not_fit() {
        let big = MAX_UPLOAD / 2;
        let mut files = vec![
            packed("a.png", "png", vec![0u8; big]),
            packed("b.png", "png", vec![0u8; big]),
            packed("c.png", "png", vec![0u8; big]),
        ];
        assert_eq!(fit_upload_budget(&mut files), 2);
        assert_eq!(files.len(), 1);

        // 装得下就一个都不丢
        let mut small = vec![packed("a.png", "png", vec![0u8; 10])];
        assert_eq!(fit_upload_budget(&mut small), 0);
        assert_eq!(small.len(), 1);
    }

    /// 混合格式装不下时，优先丢 GIF，静态 PNG 尽量全留。
    #[test]
    fn upload_budget_drops_gif_before_png() {
        // 两个各占 55% 的大 GIF + 一个小 PNG：装不下，丢一个 GIF 后就装得下
        let gif = MAX_UPLOAD * 11 / 20;
        let mut files = vec![
            packed("001_a.gif", "gif", vec![0u8; gif]),
            packed("002_b.png", "png", vec![0u8; 1024]),
            packed("003_c.gif", "gif", vec![0u8; gif]),
        ];
        let dropped = fit_upload_budget(&mut files);
        assert_eq!(dropped, 1);
        // PNG 必须留着，GIF 被丢了一个
        assert!(
            files.iter().any(|f| f.ext == "png"),
            "静态 PNG 不该被优先丢掉：{:?}",
            files.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert_eq!(files.iter().filter(|f| f.is_gif()).count(), 1);
    }

    /// 装不下时丢的是中间的大 GIF，剩下的序号前缀会留缺口 ——
    /// 缺口正好对应被丢掉的那几张（序号是原表情包里的位置）。
    #[test]
    fn upload_budget_leaves_gaps_where_middle_items_were_dropped() {
        // 60 项：静态 PNG 小，动图 GIF 大，总体积远超 45MB
        let mut files: Vec<Packed> = Vec::new();
        for i in 1..=60u32 {
            if i % 3 == 0 {
                // 大 GIF：2.25MB 一个，20 个共 45MB
                files.push(packed(
                    &format!("{i:03}_s{i}.gif"),
                    "gif",
                    vec![0u8; MAX_UPLOAD / 20],
                ));
            } else {
                files.push(packed(
                    &format!("{i:03}_s{i}.png"),
                    "png",
                    vec![0u8; 40 * 1024],
                ));
            }
        }
        let before = files.len();
        let dropped = fit_upload_budget(&mut files);
        assert!(dropped > 0, "超限了却一个都没丢");
        assert_eq!(before - dropped, files.len());

        let nums: Vec<u32> = files
            .iter()
            .map(|f| f.name[..3].parse::<u32>().unwrap())
            .collect();
        // 顺序不变，但中间有缺口（不是截掉尾部）
        assert!(
            nums.windows(2).all(|w| w[1] > w[0]),
            "顺序被打乱了：{nums:?}"
        );
        assert!(
            nums.windows(2).any(|w| w[1] > w[0] + 1),
            "丢的是中间项，应该留缺口：{nums:?}"
        );
    }

    /// 保留下来的条目顺序不变（序号前缀仍按原顺序）。
    #[test]
    fn upload_budget_keeps_original_order() {
        let gif = MAX_UPLOAD * 11 / 20;
        let mut files = vec![
            packed("001_a.gif", "gif", vec![0u8; gif]),
            packed("002_b.png", "png", vec![0u8; 1024]),
            packed("003_c.png", "png", vec![0u8; 1024]),
        ];
        fit_upload_budget(&mut files);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(
            names.windows(2).all(|w| w[0] < w[1]),
            "顺序被打乱了：{names:?}"
        );
    }

    /// 判断 GIF 不看文件名 —— 扩展名跟着数据走。
    #[test]
    fn gif_detection_follows_the_data_not_the_file_name() {
        let renamed = Packed {
            name: "001_weird.png".to_string(),
            bytes: vec![0u8; 10],
            ext: "gif",
        };
        assert!(renamed.is_gif());

        let normal = packed("001_a.gif", "png", vec![0u8; 10]);
        assert!(!normal.is_gif());
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
