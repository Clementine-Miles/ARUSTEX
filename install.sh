#!/usr/bin/env bash
# 编译并启动 ARTEX。请先准备可访问的 PostgreSQL 数据库。
set -euo pipefail
cd "$(dirname "$0")"

if [[ -z "${ARTEX_PG_DSN:-}" && ! -f "${ARTEX_CONFIG:-config.json}" ]]; then
  printf '%s\n' "请先设置 ARTEX_PG_DSN，或将 config.example.json 复制为 config.json 并填写 PostgreSQL 连接信息。" >&2
  exit 1
fi

./update.sh
exec ./start.sh "$@"
