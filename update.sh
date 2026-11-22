#!/usr/bin/env bash
# Rebuild the checked-out source, optionally fetching it first.
set -euo pipefail
cd "$(dirname "$0")"

die() { printf '%s\n' "$*" >&2; exit 1; }
usage() { printf '%s\n' "用法：./update.sh [--pull] [--backend-only]"; }

[[ "$(uname -s)" == Linux ]] || die "ARTEX source updates require Linux"
pull=false
build_args=()
for arg in "$@"; do
  case "$arg" in
    --pull) pull=true ;;
    --backend-only) build_args=(--backend-only) ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 1 ;;
  esac
done

if "$pull"; then
  [[ -e .git ]] && command -v git >/dev/null 2>&1 || die "--pull 需要 Git 工作副本和 git"
  git pull --ff-only
fi

./build.sh "${build_args[@]}"
printf '%s\n' "如 ARTEX 正在运行，请先停止，再使用原启动参数执行 ./start.sh。"
