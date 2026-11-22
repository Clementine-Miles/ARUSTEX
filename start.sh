#!/bin/sh
# Run the locally built executable in the foreground so it receives signals directly.
set -eu
cd "$(dirname "$0")"

[ "$(uname -s)" = Linux ] || { printf '%s\n' "ARTEX startup requires Linux" >&2; exit 1; }
[ -f ./artex ] && [ -x ./artex ] || {
  printf '%s\n' "[artex] 找不到可执行文件 ./artex，请先运行 ./build.sh。" >&2
  exit 1
}
exec ./artex "$@"
