#!/bin/bash
# 常驻内存测量（project.md §12 阶段 5：常驻内存 < 200 MB）
#
# 用法：scripts/measure-memory.sh [秒数]
#
# 说明：
# - 两种 GSK 渲染后端各测一次（cairo 与 gl），对应 §12 的验收要求。
# - 同时报告 VmRSS 与 Pss：
#     VmRSS 把共享库（gtk4 / libadwaita / mesa / 字体）整份计入本进程，
#     对 GTK 应用偏高；Pss 按共享比例分摊，是"这个应用真正占了多少内存"的公平口径。
# - 无显示服务器时用 xvfb-run 起一个独立的 X server。
#   **注意**：Xvfb 下 gl 后端会走 llvmpipe 软件光栅化，显存缓冲落在匿名内存里，
#   gl 的数值会明显高于真实 GPU 环境。

set -u

DURATION=${1:-26}
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
BIN="$(cd "$(dirname "$0")/.." && pwd)/${CARGO_TARGET_DIR}/release/archstore"

if [ ! -x "$BIN" ]; then
  echo "找不到 $BIN，请先运行：cargo build --release --workspace" >&2
  exit 1
fi

# 找真实进程：用 /proc/PID/exe 反查，避免匹配到 timeout/xvfb-run 包装进程
find_archstore() {
  for d in /proc/[0-9]*; do
    exe=$(readlink "$d/exe" 2>/dev/null) || continue
    if [ "$exe" = "$BIN" ]; then
      basename "$d"
      return 0
    fi
  done
  return 1
}

sample() {
  local label="$1" pid rss pss hwm thr
  pid=$(find_archstore) || { echo "  $label: 进程不存在"; return 1; }
  rss=$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status")
  pss=$(awk '/^Pss:/{print $2}' "/proc/$pid/smaps_rollup" 2>/dev/null)
  hwm=$(awk '/^VmHWM:/{print $2}' "/proc/$pid/status")
  thr=$(awk '/^Threads:/{print $2}' "/proc/$pid/status")
  printf '  %-10s VmRSS=%4d MB   Pss=%4d MB   峰值=%4d MB   线程=%s\n' \
    "$label" "$((rss/1024))" "$((${pss:-0}/1024))" "$((hwm/1024))" "$thr"
}

FAIL=0
for RENDERER in cairo gl; do
  echo "=== GSK_RENDERER=$RENDERER ==="
  export GSK_RENDERER=$RENDERER
  export ARCHSTORE_LOG=warn
  LOG=$(mktemp)
  timeout "$DURATION" xvfb-run -a "$BIN" > "$LOG" 2>&1 &
  WRAPPER=$!
  # 等 GTK 主循环起来 + 后台服务（libalpm / AUR / Flathub）加载完成
  sleep 8
  sample "启动 8s"
  sleep 5
  sample "启动 13s"
  if kill -0 "$WRAPPER" 2>/dev/null; then
    sleep 5
    sample "启动 18s"
  else
    echo "  (进程已按超时退出，跳过第三次采样)"
  fi

  CRITICAL=$(grep -c 'CRITICAL' "$LOG" 2>/dev/null || echo 0)
  echo "  CRITICAL 计数: $CRITICAL"
  [ "$CRITICAL" != "0" ] && FAIL=1

  wait $WRAPPER 2>/dev/null
  rm -f "$LOG"
  sleep 1
done

echo
echo "判定标准（§12 阶段 5）：Pss < 200 MB 且 CRITICAL = 0"
exit $FAIL
