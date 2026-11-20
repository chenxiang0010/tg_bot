#!/usr/bin/env bash
# 把机器人装成当前用户的 systemd 服务：后台常驻 + 开机自启。
# 全程不需要 sudo。
set -euo pipefail

# rlottie-sys 的构建脚本会在它 clone 的源码里 cherry-pick 一个补丁。
# 如果全局开启了 commit.gpgsign 但没有可用私钥，git 会在这里签名失败。
# 只对本次构建禁用签名，不改用户的全局 git 配置。
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=commit.gpgsign
export GIT_CONFIG_VALUE_0=false

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT="$UNIT_DIR/tg-bot.service"

if [[ ! -f "$ROOT/.env" ]]; then
    echo "缺少 $ROOT/.env —— 先 cp .env.example .env 并填上 TELOXIDE_TOKEN" >&2
    exit 1
fi

if ! grep -qE '^TELOXIDE_TOKEN=.+' "$ROOT/.env"; then
    echo "$ROOT/.env 里的 TELOXIDE_TOKEN 是空的" >&2
    exit 1
fi

# 构建期依赖：
#   rlottie 现场编译 C++ 渲染器 -> cmake / git / g++
#   ffmpeg-next + rlottie 都用 bindgen 生成 FFI -> libclang
#   ffmpeg-next 链接 libav* -> ffmpeg 的开发头文件
echo "==> 检查构建依赖"
missing=()
for tool in cmake git g++ pkg-config; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
# 注意：这里都先把输出收进变量再 grep。管道里用 grep -q 会让上游吃 SIGPIPE，
# 配上 pipefail 就会把整条管道判成失败，得到假的「缺失」结论。
ldconfig_out="$(ldconfig -p 2>/dev/null || true)"
if ! grep -q libclang <<<"$ldconfig_out" && ! command -v clang >/dev/null; then
    missing+=("libclang-dev")
fi
for lib in libavcodec libavformat libavfilter libavutil libswscale; do
    pkg-config --exists "$lib" 2>/dev/null || missing+=("${lib}-dev")
done
if ((${#missing[@]})); then
    echo "!! 缺少构建依赖：${missing[*]}" >&2
    echo "   装一下：sudo apt install ${missing[*]}" >&2
    exit 1
fi
echo "    cmake / git / g++ / libclang / libav* 开发包都在"

# libvpx-vp9 解码器决定视频贴纸能不能保住透明背景。它是 libavcodec 的编译选项，
# 装了开发包不代表一定有。这里用 ffmpeg 命令行旁证一下（同一套安装），
# 权威判断还是程序自己启动时的 probe()，日志里会写。
# 同样注意：先收进变量再 grep，别在管道里用 grep -q（见上面 SIGPIPE 那条注释）。
if command -v ffmpeg >/dev/null; then
    decoders="$(ffmpeg -hide_banner -decoders 2>/dev/null || true)"
    if grep -q libvpx-vp9 <<<"$decoders"; then
        echo "    libvpx-vp9 可用（视频贴纸能保住透明背景）"
    else
        echo "    !! 这套 libav 里没有 libvpx-vp9，视频贴纸的透明背景会丢" >&2
    fi
fi

echo "==> 编译 release 版（首次会现场编译 rlottie，要联网且需要几分钟）"

# rlottie-sys 的构建脚本每次重跑都会先 git checkout v0.2 再 cherry-pick 那个补丁。
# 上一轮 cherry-pick 改过的文件这时候是「本地修改」，git 会拒绝：
#   error: your local changes would be overwritten by cherry-pick
# 所以先把缓存下来的上游源码复位成干净的 v0.2 —— 动的只是 cargo 构建缓存里的
# 第三方 clone，补丁构建脚本会自己重新打上，不碰任何用户文件。
for src in "$ROOT"/target/*/build/rlottie-sys/*/out/rlottie-samsung-v0.2; do
    [[ -d "$src/.git" ]] || continue
    git -C "$src" reset --hard v0.2 >/dev/null 2>&1 || true
done

cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> 写入 $UNIT"
mkdir -p "$UNIT_DIR"
sed -e "s|@ROOT@|$ROOT|g" "$ROOT/deploy/tg-bot.service" >"$UNIT"

echo "==> 允许用户服务在没登录时也跑（开机自启的前提）"
loginctl enable-linger "$USER" || echo "!! enable-linger 失败，开机自启可能不生效" >&2

echo "==> 启用并（重）启动"
systemctl --user daemon-reload
systemctl --user enable tg-bot.service
# 用 restart 而不是 enable --now：已经在跑的话要换上新编的二进制
systemctl --user restart tg-bot.service

echo
systemctl --user --no-pager status tg-bot.service || true
echo
echo "看日志：journalctl --user -u tg-bot -f"
echo "重启：  systemctl --user restart tg-bot"
echo "停掉：  systemctl --user stop tg-bot"
echo "取消自启：systemctl --user disable --now tg-bot"
