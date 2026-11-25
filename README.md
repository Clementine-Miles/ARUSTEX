# ARUSTEX

Linux 原生的模型辅助记录审阅工具：一个 Rust 编写的可执行文件 `arustex`，包含常驻守护进程和终端界面，数据只存 PostgreSQL。

操作者导入记录（JSON Lines），创建任务并指定可用的数据集；规划、执行、复核三类 agent 只能通过编译进程序的 9 个只读或"提议"类操作工作，发现一律先以待确认状态保存，改动状态需要操作者在审批视图里批准。任务结束生成带来源信息的报告，可选通过 Telegram 推送状态。

本项目**不**做主动探测、流量拦截、凭据收集、漏洞利用、任意命令执行或不受限的外部工具调用，详见 [docs/operation-boundaries.md](docs/operation-boundaries.md)。

## 构建

需要 Rust 1.100.0（`rust-toolchain.toml` 已固定）和 PostgreSQL 16。

```sh
cargo build --release --locked
install -m 0755 target/release/arustex ~/.local/bin/
```

## 快速开始

```sh
mkdir -p ~/.config/arustex
cp config.example.toml ~/.config/arustex/config.toml
export ARUSTEX_DATABASE_URL='postgres://arustex@/arustex?host=/run/postgresql'

arustex config validate
arustex db migrate
arustex import records --file tests/fixtures/records/inventory.jsonl --new-dataset demo
arustex daemon &        # 建议用 systemd 用户服务常驻
arustex                 # 打开终端界面，按 ? 查看快捷键
```

示例配置只启用不联网的 mock 模型。接入真实模型时在 `[[providers]]` 中配置，密钥写在环境变量或权限为 0600 的文件里，配置文件本身不放任何密钥。

## 命令

| 命令 | 作用 |
| --- | --- |
| `arustex` / `arustex tui` | 终端界面：总览、任务、会话、审批、资产、记录、复核结果、报告、设置 |
| `arustex daemon` | 前台运行守护进程 |
| `arustex config validate` / `show` | 校验配置并解析密钥 / 打印脱敏后的生效配置 |
| `arustex db migrate` / `status` | 执行迁移 / 查看迁移状态 |
| `arustex db backup DIR` / `verify-backup DIR` / `restore DIR` | 带校验和的逻辑备份、离线校验、恢复到空库 |
| `arustex import records` | 导入 JSON Lines 记录（默认脱敏疑似密钥） |
| `arustex import legacy [--dry-run]` | 从旧版数据库只读、按白名单导入 |
| `arustex doctor` | 检查配置、数据库、迁移和守护进程连通性 |

## 文档

- [架构](docs/architecture.md)
- [操作边界](docs/operation-boundaries.md)
- [报告格式](docs/report-schema.md)
- [从旧版迁移](docs/migration.md)
- [兼容性与已知限制](docs/compatibility.md)

## 开发

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
ARUSTEX_TEST_DATABASE_URL=postgres://postgres@127.0.0.1/postgres cargo test --locked --workspace
```

数据库测试在未设置 `ARUSTEX_TEST_DATABASE_URL` 时跳过；设置 `ARUSTEX_REQUIRE_PG=1` 后跳过视为失败（CI 如此运行）。每个测试使用独立的临时数据库。

## 许可

[AGPL-3.0](LICENSE)，另见 [NOTICE](NOTICE)。
