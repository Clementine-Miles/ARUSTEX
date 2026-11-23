//go:build linux && !android

package main

import (
	"bufio"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"

	"github.com/Autumn-27/artex/config"
	"github.com/jackc/pgx/v5"
	"golang.org/x/crypto/bcrypt"
	"golang.org/x/sys/unix"
)

const resetPasswordUsage = `用法：artex reset-password [连接选项] [-p 新密码] [-y]
连接选项：
  --dsn DSN                 PostgreSQL 连接串
  --config 路径             配置文件
  -H, --host 主机           数据库地址
  -P, --port 端口           数据库端口
  -U, --user 用户           数据库用户
  -W, --db-password 密码    数据库密码
  -d, --dbname 名称         数据库名称
  --sslmode 模式            PostgreSQL TLS 模式
`

type resetPasswordOptions struct {
	database   config.Database
	configPath string
	password   string
	yes        bool
}

func parseResetPasswordOptions(args []string) (resetPasswordOptions, error) {
	var options resetPasswordOptions
	var mode, port string
	flags := flag.NewFlagSet("reset-password", flag.ContinueOnError)
	flags.SetOutput(io.Discard)
	flags.StringVar(&options.database.DSN, "dsn", "", "")
	flags.StringVar(&options.configPath, "config", "", "")
	flags.StringVar(&options.database.SSLMode, "sslmode", "", "")
	flags.StringVar(&options.database.Host, "H", "", "")
	flags.StringVar(&options.database.Host, "host", "", "")
	flags.StringVar(&port, "P", "", "")
	flags.StringVar(&port, "port", "", "")
	flags.StringVar(&options.database.User, "U", "", "")
	flags.StringVar(&options.database.User, "user", "", "")
	flags.StringVar(&options.database.Password, "W", "", "")
	flags.StringVar(&options.database.Password, "db-password", "", "")
	flags.StringVar(&options.database.DBName, "d", "", "")
	flags.StringVar(&options.database.DBName, "dbname", "", "")
	flags.StringVar(&options.password, "p", "", "")
	flags.StringVar(&options.password, "new-password", "", "")
	flags.StringVar(&mode, "m", "local", "")
	flags.StringVar(&mode, "mode", "local", "")
	flags.BoolVar(&options.yes, "y", false, "")
	flags.BoolVar(&options.yes, "yes", false, "")
	if err := flags.Parse(args); errors.Is(err, flag.ErrHelp) {
		return options, err
	} else if err != nil || flags.NArg() != 0 {
		return options, errors.New("无效的重置密码参数（使用 --help 查看用法）")
	}
	if mode != "local" {
		return options, errors.New("仅支持 local 模式")
	}
	if port != "" {
		number, err := strconv.Atoi(port)
		if err != nil || number < 1 || number > 65535 {
			return options, errors.New("数据库端口必须在 1 至 65535 之间")
		}
		options.database.Port = number
	}
	return options, nil
}

func (o resetPasswordOptions) connectionString() (string, error) {
	db := o.database
	if db.DSN != "" {
		return db.ConnectionString()
	}
	if db.Host != "" || db.User != "" || db.DBName != "" {
		if db.User == "" || db.DBName == "" {
			return "", errors.New("请同时指定数据库用户（-U）和数据库名（-d）")
		}
		return db.ConnectionString()
	}
	if dsn := strings.TrimSpace(os.Getenv("ARTEX_PG_DSN")); dsn != "" {
		return (config.Database{DSN: dsn}).ConnectionString()
	}
	cfg, err := config.ReadPath(o.configPath)
	if err != nil {
		return "", err
	}
	database := cfg.Database()
	if db.Port != 0 {
		database.Port = db.Port
	}
	if db.Password != "" {
		database.Password = db.Password
	}
	if db.SSLMode != "" {
		database.SSLMode = db.SSLMode
	}
	return database.ConnectionString()
}

// readResetLine restores the terminal after a hidden prompt, even if the operator cancels.
func readResetLine(ctx context.Context, input *bufio.Reader, prompt string, secret bool) (string, error) {
	if secret {
		defer fmt.Fprintln(os.Stderr)
		fd := int(os.Stdin.Fd())
		term, err := unix.IoctlGetTermios(fd, unix.TCGETS)
		if err == nil {
			hidden := *term
			hidden.Lflag &^= unix.ECHO
			if err := unix.IoctlSetTermios(fd, unix.TCSETS, &hidden); err != nil {
				return "", errors.New("无法关闭终端回显")
			}
			defer unix.IoctlSetTermios(fd, unix.TCSETS, term)
		} else if !errors.Is(err, unix.ENOTTY) {
			return "", errors.New("无法读取终端设置")
		}
	}
	fmt.Fprint(os.Stderr, prompt)

	type reply struct {
		line string
		err  error
	}
	read := make(chan reply, 1)
	go func() {
		line, err := input.ReadString('\n')
		read <- reply{line, err}
	}()
	select {
	case <-ctx.Done():
		return "", errors.New("已取消")
	case result := <-read:
		if result.err != nil {
			return "", errors.New("无法读取输入")
		}
		return strings.TrimSuffix(strings.TrimSuffix(result.line, "\n"), "\r"), nil
	}
}

func resetPassword(args []string) error {
	options, err := parseResetPasswordOptions(args)
	if errors.Is(err, flag.ErrHelp) {
		fmt.Print(resetPasswordUsage)
		return nil
	}
	if err != nil {
		return err
	}
	dsn, err := options.connectionString()
	if err != nil {
		return err
	}
	database, err := pgx.ParseConfig(dsn)
	if err != nil {
		return errors.New("无法解析 PostgreSQL 连接配置")
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	input := bufio.NewReader(os.Stdin)
	password := options.password
	if password == "" {
		password, err = readResetLine(ctx, input, "输入新密码（用户名固定为 ARTEX）：", true)
		if err != nil {
			return err
		}
		if password == "" {
			return errors.New("密码不能为空")
		}
		confirmation, err := readResetLine(ctx, input, "再次输入以确认：", true)
		if err != nil {
			return err
		}
		if password != confirmation {
			return errors.New("两次输入不一致")
		}
	}
	fmt.Fprintf(os.Stderr, "· 目标数据库：%q@%q:%d/%q\n", database.User, database.Host, database.Port, database.Database)
	if !options.yes {
		answer, err := readResetLine(ctx, input, "确认在该库重置 ARTEX 密码？[y/N] ", false)
		if err != nil {
			return err
		}
		if !strings.EqualFold(strings.TrimSpace(answer), "y") {
			return errors.New("已取消")
		}
	}
	hash, err := bcrypt.GenerateFromPassword([]byte(password), bcrypt.DefaultCost)
	if errors.Is(err, bcrypt.ErrPasswordTooLong) {
		return errors.New("密码长度不能超过 72 字节")
	}
	if err != nil {
		return errors.New("密码加密失败")
	}
	conn, err := pgx.ConnectConfig(ctx, database)
	if err != nil {
		return errors.New("无法连接 PostgreSQL，请检查连接配置和数据库状态")
	}
	defer conn.Close(context.Background())
	// This maintenance command never runs application migrations or creates extensions.
	if _, err := conn.Exec(ctx, `INSERT INTO settings(key, value) VALUES ('auth.password_hash', $1)
ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()`, string(hash)); err != nil {
		return errors.New("写入失败，请确认数据库已初始化且该角色有权更新 settings 表")
	}
	fmt.Println("✓ 已重置 ARTEX 管理员密码。请用用户名 ARTEX + 新密码登录（无需重启服务）。")
	return nil
}
