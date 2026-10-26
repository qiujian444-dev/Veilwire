#!/usr/bin/env bash
# 一键构建 Android APK（Windows / Git Bash）
#
# 为什么需要这个脚本：直接在项目目录（含中文路径）里跑 cargo 会在链接阶段失败，
# 所以必须先把 Rust 源码复制到纯 ASCII 目录再编译。这个坑踩过不止一次。
#
# 用法：
#   bash scripts/build-android.sh            # 构建 debug APK（按 ABI 拆分）
#   bash scripts/build-android.sh release    # 构建 release APK（debug 签名，可直接装机）
#
# 依赖（本机已装好，换机器需要重装）：
#   - Rust（msvc 工具链）+ aarch64-linux-android target
#   - JDK 17
#   - Android SDK：platforms;android-35 / build-tools;35.0.0 / ndk;27.0.12077973
#   - Gradle 8.9
set -euo pipefail

MODE="${1:-debug}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---- 可按需覆盖 ----
ANDROID_HOME="${ANDROID_HOME:-E:/android-sdk}"
# 强制用 JDK 17：本机环境变量里全局设了 C:\jdk\jdk-11.0.2，
# 用 ${JAVA_HOME:-...} 兜底会被它压住，Gradle 直接报 "requires Java 17"。
# 换机器只改这一行。
JAVA_HOME="C:/Program Files/Java/jdk-17"
GRADLE_BIN="${GRADLE_BIN:-/e/gradle/gradle-8.9/bin/gradle.bat}"
NDK_VER="${NDK_VER:-27.0.12077973}"
BUILD_DIR="${BUILD_DIR:-/c/anonchat-build}"     # 必须是纯 ASCII 路径
TARGETS="${TARGETS:-aarch64-linux-android x86_64-linux-android}"
RUST_TC="$HOME/.rustup/toolchains/stable-x86_64-pc-windows-msvc"

echo "==> 1/5 准备构建目录 $BUILD_DIR/src（ASCII 路径）"
mkdir -p "$BUILD_DIR/src"
cp -rf "$ROOT/rust/." "$BUILD_DIR/src/"

# crates.io 在国内经常拉不动，用中科大镜像。
# 放在构建副本里，不污染仓库。
mkdir -p "$BUILD_DIR/src/.cargo"
cat > "$BUILD_DIR/src/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "ustc"

[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"
EOF

export PATH="$RUST_TC/bin:$HOME/.cargo/bin:$PATH"

echo "==> 2/5 确认 Rust target（注意要指定 msvc 工具链）"
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
for T in $TARGETS; do
  if ! ls "$RUST_TC/lib/rustlib/$T" >/dev/null 2>&1; then
    rustup target add "$T" --toolchain stable-x86_64-pc-windows-msvc
  fi
done

echo "==> 3/5 交叉编译 Rust core"
NDK_BIN="$ANDROID_HOME/ndk/$NDK_VER/toolchains/llvm/prebuilt/windows-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android26-clang.cmd"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar.exe"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK_BIN/x86_64-linux-android26-clang.cmd"
export CARGO_TARGET_X86_64_LINUX_ANDROID_AR="$NDK_BIN/llvm-ar.exe"

for T in $TARGETS; do
  ( cd "$BUILD_DIR/src" && CARGO_TARGET_DIR="$BUILD_DIR/target" \
      cargo build --release -p android-jni --target "$T" )
done

echo "==> 4/5 拷贝 .so 到 jniLibs"
copy_so() {
  local t="$1" abi="$2"
  local so="$BUILD_DIR/target/$t/release/libanonchat.so"
  [ -f "$so" ] || { echo "没找到 $so"; exit 1; }
  mkdir -p "$ROOT/android/app/app/src/main/jniLibs/$abi"
  cp "$so" "$ROOT/android/app/app/src/main/jniLibs/$abi/"
  echo "    $(ls -lh "$so" | awk '{print $5}') $abi"
}
copy_so aarch64-linux-android arm64-v8a
copy_so x86_64-linux-android x86_64

echo "==> 5/5 Gradle 打包"
export JAVA_HOME
export ANDROID_HOME
if [ "$MODE" = "release" ]; then
  TASK="assembleRelease"
else
  TASK="assembleDebug"
fi
( cd "$ROOT/android/app" && "$GRADLE_BIN" --no-daemon "$TASK" )

echo
echo "==> 完成（按 ABI 拆分的产物）"
ls -lh "$ROOT/android/app/app/build/outputs/apk/"*/ 2>/dev/null | grep -E "apk|:" || true

# 按 ABI 各复制一份到 dist。arm64 给真机，x86_64 给模拟器——
# 之前只复制 arm64，dist 里的 x86_64 长期是旧包，模拟器上 `install -r`
# 会报 VERSION_DOWNGRADE / 签名不符，很容易误判成构建或签名出了问题。
SUB="debug"; [ "$MODE" = "release" ] && SUB="release"
mkdir -p "$ROOT/dist"
for pair in "arm64-v8a:arm64" "x86_64:x86_64"; do
  SRC="$ROOT/android/app/app/build/outputs/apk/$SUB/app-${pair%%:*}-$SUB.apk"
  DST="$ROOT/dist/anonchat-${pair##*:}-$SUB.apk"
  if [ -f "$SRC" ]; then
    cp "$SRC" "$DST"
    echo "    -> dist/$(basename "$DST") ($(ls -lh "$DST" | awk '{print $5}'))"
  fi
done
