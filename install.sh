#!/usr/bin/env bash
# Build and start ARTEX with an existing PostgreSQL connection.
set -euo pipefail
cd "$(dirname "$0")"

[[ "$(uname -s)" == Linux ]] || { printf '%s\n' "ARTEX installation requires Linux" >&2; exit 1; }
if [[ "${1:-}" == --help ]]; then
  printf '%s\n' "用法：./install.sh [--backend-only] [ARTEX 启动参数]"
  exit 0
fi

config_file="${ARTEX_CONFIG:-config.json}"
if [[ -z "${ARTEX_PG_DSN:-}" && ( ! -f "$config_file" || ! -r "$config_file" ) ]]; then
  printf '%s\n' "请先设置 ARTEX_PG_DSN，或将 config.example.json 复制为 config.json 并填写 PostgreSQL 连接信息。" >&2
  exit 1
fi

build_args=()
if [[ "${1:-}" == --backend-only ]]; then
  build_args+=(--backend-only)
  shift
fi
./build.sh "${build_args[@]}"
exec ./start.sh "$@"
