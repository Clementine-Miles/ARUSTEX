#!/usr/bin/env bash
# 从当前源码编译 ARTEX；使用 --pull 才先同步 Git。
set -euo pipefail
cd "$(dirname "$0")"

info(){ printf '\033[36m[*]\033[0m %s\n' "$*"; }
ok(){   printf '\033[32m[+]\033[0m %s\n' "$*"; }
warn(){ printf '\033[33m[!]\033[0m %s\n' "$*" >&2; }
die(){  printf '\033[31m[x]\033[0m %s\n' "$*" >&2; exit 1; }

case "$#:${1:-}" in
  0:) ;;
  1:--pull)
    [[ -e .git ]] && command -v git >/dev/null 2>&1 || die "--pull 需要 Git 工作副本和 git"
    git pull --ff-only
    ;;
  1:-h|1:--help)
    printf '%s\n' "用法：./update.sh [--pull]"
    exit 0
    ;;
  *) die "用法：./update.sh [--pull]" ;;
esac

command -v go >/dev/null 2>&1 || die "未检测到 Go：请安装 go.mod 要求的版本。"
[[ ! -d ./artex ]] || die "./artex 是目录，不能写入可执行文件"

# 先在同一文件系统上生成新二进制，编译失败时保留现有版本。
build_dir="$(mktemp -d ./.artex-build.XXXXXX)"
trap 'rm -rf "$build_dir"' EXIT
if command -v npm >/dev/null 2>&1; then
  info "构建前端静态产物…"
  (cd web && npm ci && npm run build:static)
  rm -rf server/webui/dist
  mkdir -p server/webui/dist
  cp -R web/out/. server/webui/dist/
  info "编译内嵌单二进制…"
  CGO_ENABLED=0 go build -tags embedui -trimpath -o "$build_dir/artex" ./cmd/artex
else
  warn "未检测到 npm：仅编译后端（前端可另跑 npm run dev）"
  CGO_ENABLED=0 go build -o "$build_dir/artex" ./cmd/artex
fi
mv -f "$build_dir/artex" ./artex

ok "编译完成 → ./artex"
info "启动或重启 ARTEX 后使用此版本（启动时会自动迁移数据库 schema）。"
