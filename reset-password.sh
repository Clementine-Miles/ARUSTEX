#!/usr/bin/env bash
# =============================================================================
# Reset the ARTEX administrator password.
#
# The administrator username is ARTEX. PostgreSQL pgcrypto generates a bcrypt
# hash compatible with the backend and stores it as settings.auth.password_hash.
#
# Connect using the local psql client. Connection precedence is CLI options,
# ARTEX_PG_DSN, then the config file (--config > ARTEX_CONFIG > config.json).
#
# Examples:
#   ./reset-password.sh                          # Prompt for the new password.
#   ./reset-password.sh -p 'NewPass!'            # Supply the new password explicitly.
#   ./reset-password.sh --dsn postgres://u:p@h:5432/artex
#   ./reset-password.sh -H 127.0.0.1 -P 5433 -U autopentest -W pass -d artex
#
# psql reads the new password from the environment with \getenv and quotes it as
# a SQL literal. A separate database password uses PGPASSWORD. A DSN is passed
# as a psql argument.
# =============================================================================
set -euo pipefail

PASS_KEY="auth.password_hash"
BCRYPT_COST=10

DSN=""
HOST="" PORT="" USER="" DBPASS="" DBNAME="" SSLMODE=""
CONFIG=""
NEWPASS=""
ASSUME_YES=0

die() { echo "错误：$*" >&2; exit 1; }
info() { echo "· $*" >&2; }

usage() {
  cat <<'USAGE'
用法：./reset-password.sh [连接选项] [-p 新密码] [-y]
连接选项：
  --dsn DSN                 PostgreSQL 连接串
  --config 路径             配置文件
  -H, --host 主机           数据库地址
  -P, --port 端口           数据库端口
  -U, --user 用户           数据库用户
  -W, --db-password 密码    数据库密码
  -d, --dbname 名称         数据库名称
  --sslmode 模式            PostgreSQL TLS 模式
USAGE
}

# Parse command-line options.
while [[ $# -gt 0 ]]; do
  case "$1" in
    -m|--mode)        [[ "${2:-}" == "local" ]] || die "仅支持本机 psql 连接"; shift 2 ;;
    --dsn)            DSN="${2:-}"; shift 2 ;;
    -H|--host)        HOST="${2:-}"; shift 2 ;;
    -P|--port)        PORT="${2:-}"; shift 2 ;;
    -U|--user)        USER="${2:-}"; shift 2 ;;
    -W|--db-password) DBPASS="${2:-}"; shift 2 ;;
    -d|--dbname)      DBNAME="${2:-}"; shift 2 ;;
    --sslmode)        SSLMODE="${2:-}"; shift 2 ;;
    --config)         CONFIG="${2:-}"; shift 2 ;;
    -p|--new-password) NEWPASS="${2:-}"; shift 2 ;;
    -y|--yes)         ASSUME_YES=1; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) die "未知参数：$1（-h 查看用法）" ;;
  esac
done

# Read database settings from JSON when no connection was supplied. The grep
# fallback supports simple string and numeric fields when Python is absent.
read_config_json() {
  local path="$1"
  [[ -f "$path" ]] || return 1
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$path" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1])).get("database", {})
except Exception:
    sys.exit(1)
# A complete DSN takes precedence over individual fields.
if d.get("dsn"):
    print("DSN\t" + d["dsn"]); sys.exit(0)
for k in ("host","port","user","password","dbname","sslmode"):
    if d.get(k) is not None:
        print(k.upper() + "\t" + str(d[k]))
PY
  else
    local k
    for k in host port user password dbname sslmode; do
      local v
      v=$(grep -oE "\"$k\"[[:space:]]*:[[:space:]]*(\"[^\"]*\"|[0-9]+)" "$path" 2>/dev/null \
            | head -1 | sed -E "s/.*:[[:space:]]*//; s/^\"//; s/\"$//") || true
      [[ -n "$v" ]] && echo -e "${k^^}\t$v"
    done
  fi
}

apply_config_fields() {
  local line key val
  while IFS=$'\t' read -r key val; do
    [[ -z "$key" ]] && continue
    case "$key" in
      DSN)      [[ -z "$DSN" ]] && DSN="$val" ;;
      HOST)     [[ -z "$HOST" ]] && HOST="$val" ;;
      PORT)     [[ -z "$PORT" ]] && PORT="$val" ;;
      USER)     [[ -z "$USER" ]] && USER="$val" ;;
      PASSWORD) [[ -z "$DBPASS" ]] && DBPASS="$val" ;;
      DBNAME)   [[ -z "$DBNAME" ]] && DBNAME="$val" ;;
      SSLMODE)  [[ -z "$SSLMODE" ]] && SSLMODE="$val" ;;
    esac
  done
}

# Prompt for the new password if omitted.
if [[ -z "$NEWPASS" ]]; then
  read -r -s -p "输入新密码（用户名固定为 ARTEX）：" NEWPASS; echo >&2
  [[ -n "$NEWPASS" ]] || die "密码不能为空"
  read -r -s -p "再次输入以确认：" NEWPASS2; echo >&2
  [[ "$NEWPASS" == "$NEWPASS2" ]] || die "两次输入不一致"
fi
[[ -n "$NEWPASS" ]] || die "密码不能为空"

# Pass the new password to psql through its environment.
export ARTEX_RESET_NEWPASS="$NEWPASS"

# Upsert the bcrypt hash using a quoted SQL literal. If pgcrypto is missing,
# the database role needs permission to create the extension.
SQL=$(cat <<SQL
\\set ON_ERROR_STOP on
\\getenv newpw ARTEX_RESET_NEWPASS
CREATE EXTENSION IF NOT EXISTS pgcrypto;
INSERT INTO settings(key, value)
VALUES ('$PASS_KEY', crypt(:'newpw', gen_salt('bf', $BCRYPT_COST)))
ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now();
SQL
)

# Fill connection details that were omitted on the command line.
if [[ -z "$DSN" && -z "$HOST$USER$DBNAME" ]]; then
  [[ -n "${ARTEX_PG_DSN:-}" ]] && DSN="$ARTEX_PG_DSN"
fi
if [[ -z "$DSN" && -z "$HOST$USER$DBNAME" ]]; then
  cfg="${CONFIG:-${ARTEX_CONFIG:-config.json}}"
  if [[ -f "$cfg" ]]; then
    info "从 $cfg 读取数据库配置"
    apply_config_fields < <(read_config_json "$cfg")
  fi
fi

command -v psql >/dev/null 2>&1 || die "本机未找到 psql（请安装 postgresql-client）"

declare -a PSQL_ARGS=()
if [[ -n "$DSN" ]]; then
  PSQL_ARGS=("$DSN")
  target="已配置的 DSN（内容隐藏）"
else
  [[ -n "$USER"   ]] || die "缺少数据库用户（-U）或有效的 config.json/DSN"
  [[ -n "$DBNAME" ]] || die "缺少数据库名（-d）或有效的 config.json/DSN"
  HOST="${HOST:-127.0.0.1}"; PORT="${PORT:-5432}"; SSLMODE="${SSLMODE:-disable}"
  PSQL_ARGS=(-h "$HOST" -p "$PORT" -U "$USER" -d "$DBNAME")
  [[ -n "$SSLMODE" ]] && export PGSSLMODE="$SSLMODE"
  [[ -n "$DBPASS" ]] && export PGPASSWORD="$DBPASS"
  target="$USER@$HOST:$PORT/$DBNAME"
fi

info "目标数据库：$target"
if [[ "$ASSUME_YES" -ne 1 ]]; then
  read -r -p "确认在该库重置 ARTEX 密码？[y/N] " ans
  [[ "$ans" == "y" || "$ans" == "Y" ]] || die "已取消"
fi

if ! printf '%s\n' "$SQL" | psql "${PSQL_ARGS[@]}" -v ON_ERROR_STOP=1 -q >/dev/null; then
  die "写入失败。若报 pgcrypto 权限/缺失，请用具备建扩展权限的角色，或先手动执行 CREATE EXTENSION pgcrypto。"
fi

unset ARTEX_RESET_NEWPASS
echo "✓ 已重置 ARTEX 管理员密码。请用用户名 ARTEX + 新密码登录（无需重启服务）。"
