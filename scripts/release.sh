#!/usr/bin/env bash
# 打一个"用户能直接下载"的发布版本。
#
# 为什么 APK 不放仓库、放 Releases：
#   每个 APK 26MB，dist/ 里已经攒了 20 多个版本（700MB+）。进了 git 就是
#   永久历史，每次 clone 都要拉全量，而且删不掉（除非 rewrite history）。
#   Releases 的附件不占仓库，还能给"最新版"一个稳定直链：
#   https://github.com/<owner>/<repo>/releases/latest/download/<asset>
#
# 用法：
#   bash scripts/release.sh 0.13.1            # 构建 + 校验和 + 打 tag + 上传
#   bash scripts/release.sh 0.13.1 --dry-run  # 只构建与打包校验和，不碰 git / GitHub
#
# 前置：
#   - 工作区必须干净（不带未提交的改动发版）
#   - 版本号必须与 android/app/app/build.gradle.kts 的 versionName 一致
#   - 上传需要一个已认证的 GitHub CLI（gh auth login）；没有 gh 会打印手动步骤
#   - 建议先配好正式签名（否则签名是随机的 debug key，用户没法覆盖升级）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-}"
DRY_RUN="${2:-}"

if [ -z "$VERSION" ]; then
  echo "用法: bash scripts/release.sh <版本号> [--dry-run]"
  echo "例:   bash scripts/release.sh 0.13.1"
  exit 2
fi

OUT="$ROOT/dist/release-$VERSION"
TAG="v$VERSION"

echo "==> 1/6 检查版本号与工作区"
GRADLE_VERSION="$(sed -n 's/.*versionName = "\([^"]*\)".*/\1/p' "$ROOT/android/app/app/build.gradle.kts" | head -1)"
if [ "$GRADLE_VERSION" != "$VERSION" ]; then
  echo "版本不一致：build.gradle.kts 是 $GRADLE_VERSION，你传的是 $VERSION"
  echo "先改 android/app/app/build.gradle.kts 的 versionCode/versionName 再发版"
  exit 1
fi
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "工作区有未提交的改动，拒绝发版（否则包里的代码和你 tag 的 commit 对不上）"
  git -C "$ROOT" status --short | head -20
  exit 1
fi
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
echo "    版本 $VERSION，commit $COMMIT"

echo "==> 2/6 构建 release"
bash "$ROOT/scripts/build-android.sh" release

echo "==> 3/6 归档产物与校验和"
mkdir -p "$OUT"
cp "$ROOT/dist/anonchat-arm64-release.apk"  "$OUT/veilwire-$VERSION-arm64.apk"
cp "$ROOT/dist/anonchat-x86_64-release.apk" "$OUT/veilwire-$VERSION-x86_64.apk"
# 再放一份不带版本号的副本。GitHub 的 releases/latest/download/<资产名> 是按
# **文件名**解析的：只传带版本号的包就没有稳定的"最新版"直链。
# 代价是每个 Release 多存一份（约 53MB），换来一条能写死在 README 里、
# 长期有效的下载地址。
cp "$OUT/veilwire-$VERSION-arm64.apk"  "$OUT/veilwire-arm64.apk"
cp "$OUT/veilwire-$VERSION-x86_64.apk" "$OUT/veilwire-x86_64.apk"
(
  cd "$OUT"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum ./*.apk > SHA256SUMS
  else
    shasum -a 256 ./*.apk > SHA256SUMS     # macOS
  fi
  cat SHA256SUMS
)

echo "==> 4/6 记录构建元数据"
{
  echo "version:  $VERSION"
  echo "commit:   $COMMIT ($(git -C "$ROOT" rev-parse HEAD))"
  echo "built:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "rustc:    $(rustc --version 2>/dev/null || echo 'n/a')"
  echo "java:     $(java -version 2>&1 | head -1 || echo 'n/a')"
  echo "gradle:   ${GRADLE_BIN:-gradle 8.9}"
  echo
  echo "核对签名（debug 签名时这里会显示 CN=Android Debug）："
  APKSIGNER="$(command -v apksigner || true)"
  if [ -z "$APKSIGNER" ]; then
    # Windows 上文件叫 apksigner.bat 且往往没有 +x 位，所以用 -f 判断；
    # 按版本倒序取最新的 build-tools（ls 的默认字典序会把 34 排在 35 前面）
    for p in $(ls -d "${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"/build-tools/*/apksigner* 2>/dev/null | sort -Vr); do
      [ -f "$p" ] && APKSIGNER="$p" && break
    done
  fi
  if [ -n "$APKSIGNER" ]; then
    "$APKSIGNER" verify --print-certs "$OUT/veilwire-$VERSION-arm64.apk" 2>/dev/null | head -4
  else
    echo "  (没找到 apksigner，手动跑：apksigner verify --print-certs <apk>)"
  fi
} > "$OUT/BUILD-INFO.txt"
cat "$OUT/BUILD-INFO.txt"

echo "==> 5/6 签名检查"
if grep -q "CN=Android Debug" "$OUT/BUILD-INFO.txt" 2>/dev/null; then
  echo "    ⚠️  当前是 debug 签名："
  echo "       - 换台机器编出来的包签名不同，用户覆盖安装会失败（INSTALL_FAILED_UPDATE_INCOMPATIBLE）"
  echo "       - debug keystore 的密码是公开的，任何人都能签同包名的包冒充更新"
  echo "       → 对外分发前请配置 VEILWIRE_KEYSTORE 等环境变量（见 build.gradle.kts 注释）"
fi

if [ "$DRY_RUN" = "--dry-run" ]; then
  echo "==> 6/6 --dry-run，跳过 tag 与上传。产物在 $OUT"
  exit 0
fi

echo "==> 6/6 打 tag 并上传到 GitHub Releases"
git -C "$ROOT" tag -a "$TAG" -m "Veilwire $VERSION"
git -C "$ROOT" push origin "$TAG"

if command -v gh >/dev/null 2>&1; then
  gh release create "$TAG" \
    "$OUT/veilwire-$VERSION-arm64.apk" \
    "$OUT/veilwire-$VERSION-x86_64.apk" \
    "$OUT/veilwire-arm64.apk" \
    "$OUT/veilwire-x86_64.apk" \
    "$OUT/SHA256SUMS" \
    "$OUT/BUILD-INFO.txt" \
    --title "Veilwire $VERSION" \
    --notes "见 CHANGELOG.md 与 README。校验：sha256sum -c SHA256SUMS"
else
  echo "    没装 GitHub CLI，手动上传这些文件到 tag $TAG 的 Release："
  ls -1 "$OUT"
  echo "    或用：gh release create $TAG <assets...> --title \"Veilwire $VERSION\""
fi

echo
echo "完成。稳定下载直链（永远指向最新版，因为文件名不含版本号）："
echo "  https://github.com/<owner>/<repo>/releases/latest/download/veilwire-arm64.apk"
echo "  https://github.com/<owner>/<repo>/releases/latest/download/veilwire-x86_64.apk"
echo "本次版本的固定直链（不随新版本变化）："
echo "  https://github.com/<owner>/<repo>/releases/download/$TAG/veilwire-$VERSION-arm64.apk"
