#!/usr/bin/env bash
# 跑 Android 端的 JVM 单元测试（MsgStore 等纯逻辑层）。
#
# 为什么不在原地跑：项目路径含中文（去中心化匿名聊天软件），Gradle 的 test worker
# 在非 ASCII 路径下解析 classpath 会失败——现象是 ClassNotFoundException 明明
# 编译产物就在 build/tmp/kotlin-classes 下，worker 却找不到。
# （assemble 不受影响，只有 test 任务踩这个坑。）
#
# 所以把源码复制到纯 ASCII 目录再跑，和 Rust 那边的构建目录是同一个思路。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="${STAGE:-/c/anonchat-build/atest}"

# 必须硬编码：系统全局 JAVA_HOME 是 C:\jdk\jdk-11.0.2，用 ${JAVA_HOME:-...} 会被它压住
export JAVA_HOME="C:/Program Files/Java/jdk-17"
export ANDROID_HOME="${ANDROID_HOME:-E:/android-sdk}"
GRADLE_BIN="${GRADLE_BIN:-/e/gradle/gradle-8.9/bin/gradle.bat}"

echo "==> 复制源码到 $STAGE（ASCII 路径）"
mkdir -p "$STAGE"
( cd "$ROOT/android" && tar -cf - --exclude=build --exclude=.gradle --exclude=.cxx . ) \
  | ( cd "$STAGE" && tar -xf - )

echo "==> 运行 :app:testDebugUnitTest"
( cd "$STAGE/app" && "$GRADLE_BIN" --no-daemon :app:testDebugUnitTest "$@" )

echo "==> 结果"
report="$STAGE/app/app/build/test-results/testDebugUnitTest"
if ls "$report"/*.xml >/dev/null 2>&1; then
  grep -h -o 'tests="[0-9]*" skipped="[0-9]*" failures="[0-9]*" errors="[0-9]*"' "$report"/*.xml || true
fi
