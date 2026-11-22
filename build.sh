#!/usr/bin/env bash
# Build ARTEX from the current source tree on Linux.
set -euo pipefail
cd "$(dirname "$0")"

die() { printf '%s\n' "$*" >&2; exit 1; }
usage() { printf '%s\n' "用法：./build.sh [--backend-only]"; }

[[ "$(uname -s)" == Linux ]] || die "ARTEX builds require Linux"
case "$#:${1:-}" in
  0:) tags=(-tags embedui) ;;
  1:--backend-only) tags=() ;;
  1:-h|1:--help) usage; exit 0 ;;
  *) usage >&2; exit 1 ;;
esac

command -v go >/dev/null 2>&1 || die "未检测到 Go：请安装 go.mod 要求的版本。"
export GOTOOLCHAIN=local GOOS=linux CGO_ENABLED=0
GOARCH="$(go env GOHOSTARCH)"
export GOARCH
case "$GOARCH" in
  amd64|arm64) ;;
  *) die "Unsupported Linux architecture: $GOARCH (expected amd64 or arm64)" ;;
esac
go list -mod=readonly -m >/dev/null
[[ ! -d ./artex ]] || die "./artex 是目录，不能写入可执行文件"

if (( ${#tags[@]} )); then
  command -v npm >/dev/null 2>&1 || die "未检测到 npm（前端静态构建需要 Node.js/npm）"
  node -e 'process.exit(Number(process.versions.node.split(".")[0]) < 22 ? 1 : 0)' ||
    die "前端静态构建需要 Node.js 22 或更高版本。"
  printf '%s\n' "构建前端静态产物…"
  (cd web && npm ci --ignore-scripts --no-audit --no-fund && NEXT_TELEMETRY_DISABLED=1 npm run build:static)
  rm -rf server/webui/dist
  mkdir -p server/webui/dist
  cp -R web/out/. server/webui/dist/
fi

version="$(git describe --tags --always --dirty 2>/dev/null)" || version=dev

# Stage the output on the same filesystem so a failed build leaves the previous executable intact.
build_dir="$(mktemp -d ./.artex-build.XXXXXX)"
trap 'rm -rf "$build_dir"' EXIT
printf '%s\n' "编译 linux/${GOARCH}，版本 ${version}"
go build -mod=readonly -trimpath "${tags[@]}" -ldflags "-X main.version=$version" -o "$build_dir/artex" ./cmd/artex
mv -fT -- "$build_dir/artex" ./artex
printf '%s\n' "编译完成 → ./artex"
