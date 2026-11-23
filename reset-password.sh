#!/bin/sh
# Use the locally built application so password reset shares startup configuration.
set -eu

[ "$(uname -s)" = Linux ] || { printf '%s\n' "ARTEX password reset requires Linux" >&2; exit 1; }
artex="$(dirname "$0")/artex"
[ -f "$artex" ] && [ -x "$artex" ] || {
  printf '%s\n' "[artex] 找不到可执行文件 artex，请先运行 ./build.sh。" >&2
  exit 1
}
exec "$artex" reset-password "$@"
