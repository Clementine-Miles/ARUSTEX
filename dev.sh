#!/usr/bin/env bash
# Run the Go backend and Next.js development server together.
set -euo pipefail
cd "$(dirname "$0")"

die() { printf '%s\n' "$*" >&2; exit 1; }
[[ "$(uname -s)" == Linux ]] || die "ARTEX development requires Linux"
command -v go >/dev/null 2>&1 || die "未检测到 Go：请安装 go.mod 要求的版本。"
command -v npm >/dev/null 2>&1 || die "未检测到 npm（前端开发需要 Node.js/npm）"
[[ -x web/node_modules/.bin/next ]] || die "请先在 web 目录运行 npm ci。"

export GOTOOLCHAIN=local GOOS=linux
GOARCH="$(go env GOHOSTARCH)"
export GOARCH
export AUTOPENTEST_API="${AUTOPENTEST_API:-http://127.0.0.1:8787}"
export NEXT_TELEMETRY_DISABLED=1

# Put each development job in its own process group so cleanup reaches its descendants.
set -m
backend_pid=
frontend_pid=
cleanup() {
  if [[ -n "$backend_pid" ]]; then kill -TERM -- "-$backend_pid" 2>/dev/null || true; fi
  if [[ -n "$frontend_pid" ]]; then kill -TERM -- "-$frontend_pid" 2>/dev/null || true; fi
  wait
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

go run -mod=readonly ./cmd/artex -addr 127.0.0.1:8787 -proxy 127.0.0.1:8788 &
backend_pid=$!
(cd web && exec npm run dev -- --hostname 127.0.0.1 --port 5173) &
frontend_pid=$!

echo "[dev] 后端 :8787 / 代理 :8788 / 前端 http://localhost:5173  (Ctrl-C 退出)"
wait -n "$backend_pid" "$frontend_pid"
