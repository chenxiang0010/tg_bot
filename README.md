# 表情包转图标机器人

Telegram 机器人：发一张贴纸、图片或动图给它，它解析出原图、转成方形图标再发回来。
输出格式跟着素材本身走：静态的出 PNG，动图的出 GIF。

## 功能

**单张转换** —— 直接把素材发给它：

| 输入 | 输出 |
| --- | --- |
| 静态贴纸 (webp)、照片、图片文件 | PNG |
| 视频贴纸 (webm)、动态贴纸 (tgs)、GIF 动图 | GIF（保留透明背景） |

格式不用指定，由素材自身类型决定：静态的走 PNG 那条路，动图的走 GIF 那条路。
不管哪种，都会**等比缩放并用透明边补成正方形**：不拉变形，也不把小图放大成糊图。

**整套转换** —— `/pack`：

| 命令 | 说明 |
| --- | --- |
| `/pack <名称>` | 打包成一个 zip 发回（默认，最多 120 张） |
| `/pack <名称> png` | 整套只要静态 PNG（动图取缩略图，体积小） |
| `/pack <名称> files` | 一张张单独发，最多 30 张 |

整套默认也是**按类型分流**：一套里静态贴纸出 PNG、动图出 GIF，混着装在同一个 zip 里。
想要整套统一成静态的（比如 GIF 太多装不下），显式加 `png`。

> `gif` 这个关键字还认，但它现在的意思就是「默认」—— 静态的照样出 PNG。
> 跟 `png` 同时出现时由 `png` 说了算，跟参数顺序无关。

名称就是分享链接 `t.me/addstickers/xxx` 后面那段，直接粘整条链接也认。
也可以回复某张贴纸再发 `/pack`，名称从被回复的消息里取。

## 运行

```bash
cp .env.example .env      # 填入 @BotFather 给的 token
cargo run --release
```

`.env` 已在 `.gitignore` 里，token 不会被提交。

### 后台常驻 + 开机自启

```bash
./deploy/install.sh
```

装成当前用户的 systemd 服务，不需要 sudo。脚本会编译 release、写入
`~/.config/systemd/user/tg-bot.service`、开 linger（保证没登录也能跑）、启用并启动。

```bash
journalctl --user -u tg-bot -f      # 看日志
systemctl --user restart tg-bot     # 改了 .env 后重启
systemctl --user disable --now tg-bot   # 停掉并取消自启
```

## 依赖

**构建期**（`rlottie` 会现场编译 C++ 渲染器，需要联网 clone 源码）：

```bash
sudo apt install cmake git g++ libclang-dev
```

`deploy/install.sh` 会先检查这几个，缺了直接给出安装命令再退出。rlottie 是**静态链入**的，
编出来的二进制运行时不需要它，只多一个 `libstdc++.so.6`。

动图转换用的是 **ffmpeg-next**（libav* 的 Rust 绑定），编解码在进程内完成，不起子进程。
所以构建期还需要 ffmpeg 的开发头文件：

```bash
sudo apt install libavcodec-dev libavformat-dev libavfilter-dev libavutil-dev libswscale-dev
```

**运行期**：只要对应的 `libav*.so` 在（装了上面的包自然就在）。不需要 `ffmpeg` 可执行文件。

> libavcodec 必须带 **libvpx-vp9** 解码器。内置的 `vp9` 解码器会把 Telegram 视频贴纸的
> 透明通道解成全不透明，只有 libvpx 那个能正确还原 alpha，所以代码里对 webm 显式选了它。
> 启动时会探测，缺了会在日志里警告并照常运行（只是丢透明背景）。

## 日志和临时文件

**日志**默认打到 stdout：

- 跑在 systemd 下由 journald 收走 —— `journalctl --user -u tg-bot -f`
- 直接 `cargo run` 就打在终端上
- 想额外写文件就设 `LOG_FILE=/path/to/tg_bot.log`（会追加写，自动关掉 ANSI 颜色）
- 级别用 `RUST_LOG` 控制，默认 `info`

日志里只有 chat id 和错误文字，不含任何图片内容。

**临时文件：没有。** 下载、解码、缩放、GIF 编码、zip 打包全在内存里完成：

- Telegram 文件下载进 `Vec<u8>`
- 上传用 `InputFile::memory()`，不是从磁盘路径读
- ffmpeg 用 `pipe:0` / `pipe:1` 管道进出
- zip 写进 `Cursor<Vec<u8>>`

全项目唯一写文件系统的地方就是上面那个可选的 `LOG_FILE`。

## 代码结构

- `src/icon.rs` —— 静态图转换：解码 → 等比缩放居中 → PNG。不碰网络，可单独测试。
- `src/lottie.rs` —— tgs 渲染：gunzip → rlottie 逐帧渲成 BGRA 裸帧。
- `src/video.rs` —— GIF 编码：解码 → 滤镜图 → GIF 封装，全走 libav 进程内 API，含体积降级阶梯。
- `src/main.rs` —— 机器人：命令路由、从消息里找素材、下载、打包、发送。
  输出格式由 `Format` 策略决定（`Auto` 按素材类型分流、`Static` 强制纯 PNG），
  整张决策表在 `output_of` 里，静态降级和 GIF 编码失败的兜底也都在那儿。

三种动图殊途同归到同一条 GIF 流水线：

```
webm ──────────────────────┐
mp4 / gif ─────────────────┤→ libavfilter（抽帧 + 缩放补方 + 调色板）→ GIF 编码 → 封装
tgs → rlottie 渲成 BGRA 裸帧 ┘
```

滤镜链仍然是那串 ffmpeg 滤镜字符串，交给 `avfilter_graph_parse` 解析 —— 所以调色板那套
调好的参数原样复用，没有手搭滤镜图。

编解码是纯 CPU 活，整个转换丢进 `spawn_blocking`；整套转换限制 4 路并发。

```bash
cargo test
```

### GIF 体积控制

GIF 没有帧间压缩，很容易几十 MB，所以按阶梯降级：从 512px/20fps 开始，超过体积上限就往下
走一档（384/16 → 320/15 → 256/12），取第一个装得下的。整套打包时从 320/15 起步，上限更严
（每张 1 MB）。

单张转换用宽松档（512/20，每张 4 MB），整套用紧凑档（320/15，每张 1 MB）——
因为一套几十张，每张省一点总体就差很多。`/pack` 混合格式时，这个每张上限只作用在 GIF 上，
静态 PNG 不受限。

tgs 是矢量的，直接按阶梯首档的尺寸渲染，比先渲大图再缩更清楚。

实测（都是 512×512 的真实素材）：

| 素材 | 结果 |
| --- | --- |
| 3 秒视频贴纸（满屏照片内容，GIF 最坏情况） | 512/20 默认抖动 6.61 MB → 换 bayer 抖动 4.75 MB → 降到 384/16 得 2.50 MB |
| tgs 动态贴纸（UtyaDuck，60 帧） | 512/20 直接 **1.20 MB** |
| tgs 动态贴纸（HotCherry，30 帧） | 512/20 直接 **0.39 MB** |

tgs 出来的 GIF 比视频贴纸小得多 —— Lottie 是平涂矢量图形，正好是 256 色调色板的舒适区。

## 已知限制

- `getFile` 只能下载 20 MB 以内的文件，机器人上传单个文件上限 50 MB（代码里留到 45 MB）。
- 动图出 GIF 很容易超上传上限被截断，`/pack` 混合格式时尤其明显；整套都要静态就显式加 `png`。
- `/pack` 的格式选择记在参数里，不保存状态；重启不影响。

## 两个 Telegram 的坑

**sendDocument 会把 GIF 偷偷转成 MP4。** Telegram 对 multipart 上传做服务端内容类型探测，
认出 GIF 就转成 MP4 动画（这样能在聊天里自动播放），用户下下来是个 `.gif` 名字的 mp4 文件。
必须显式关掉：

```rust
bot.send_document(chat, file).disable_content_type_detection(true)
```

代价比想象中小：实测关掉探测后，Telegram 客户端仍然能正常播放这个 GIF，同时下下来是货真价实的
GIF 文件。媒体组（`sendMediaGroup`）里的文档 Telegram 强制不做探测，所以整套 `files` 模式本来
就没问题，只有落单那张走 sendDocument 的需要处理。

**视频贴纸的透明通道只有 libvpx-vp9 解得出来。** 见上面「依赖」一节。

## 直接用 libav 时踩的三个坑

都是换成 ffmpeg-next 之后才会遇到的，命令行版本不存在这些问题：

**1. buffersink 取帧必须用干净的 AVFrame。** `av_buffersink_get_frame` 内部是
`av_frame_move_ref`，**不会**先 unref 目标帧。复用同一个 frame 反复取，上一帧的缓冲就漏了。
实测泄漏 1.4 MB/次且随次数线性增长，跑一千次就是 1.4 GB。修法是每轮新建一个 frame。
（解码那边不用管，`avcodec_receive_frame` 会自己先 unref。）

**2. dyn_buf 不能让 `Output::drop` 去关。** 输出用 `avio_open_dyn_buf` 写内存，
但它只能用 `avio_close_dyn_buf` 收尾；而 ffmpeg-next 的 `Output::drop` 会无条件
`avio_close(pb)`，撞上就是 `free(): invalid pointer`。所以取完数据必须把 `pb` 置空，
并且给 `Encoded` 加 `Drop` 兜住中途出错没走到收尾的情况。

**3. EOF 的时间戳单位是输入流的 time_base。** 收尾要用 `av_buffersrc_close(pts)` 而不是
flush，否则 `fps` 滤镜不知道末帧该持续多久会把它丢掉。但 pts 单位随来源变：裸帧是
`1/fps`（相邻间隔 1），webm 常见 `1/1000`（间隔几十）。写死 `+1` 会把解码来的末帧砍没，
得按相邻两帧的间隔推算。
