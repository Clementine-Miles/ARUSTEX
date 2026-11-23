//go:build linux && !android

// Command artex runs the exploration engine and HTTP server backed by
// PostgreSQL and the local traffic store.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"github.com/Autumn-27/artex/agent"
	"github.com/Autumn-27/artex/config"
	"github.com/Autumn-27/artex/server"
)

// version can be set at build time with -ldflags "-X main.version=<version>".
var version = "dev"

const banner = `
    _    ____ _____ _______  __
   / \  |  _ \_   _| ____\ \/ /
  / _ \ | |_) || | |  _|  \  /
 / ___ \|  _ < | | | |___ /  \
/_/   \_\_| \_\|_| |_____/_/\_\
`

// printBanner writes the startup banner + version/runtime info to stdout.
func printBanner(addr string) {
	fmt.Print(banner)
	fmt.Println("  AI 自主渗透测试系统")
	fmt.Printf("  版本 %s  ·  %s/%s  ·  %s  ·  监听 %s\n\n",
		version, runtime.GOOS, runtime.GOARCH, runtime.Version(), addr)
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "reset-password" {
		if err := resetPassword(os.Args[2:]); err != nil {
			log.Fatal(err)
		}
		return
	}
	var (
		addr    = flag.String("addr", ":8787", "HTTP listen address")
		dataDir = flag.String("data", filepath.Join(config.BaseDir(), "data"), "data directory for SQLite stores (default: data/ next to the executable)")
		proxy   = flag.String("proxy", "127.0.0.1:8788", "traffic recording proxy address (empty to disable)")
		migrate = flag.String("migrate-config", "", "write a TOML copy of the specified legacy JSON configuration and exit")
	)
	flag.Parse()
	if *migrate != "" {
		destination := strings.TrimSuffix(*migrate, filepath.Ext(*migrate)) + ".toml"
		if err := config.MigrateLegacy(*migrate, destination); err != nil {
			log.Fatal(err)
		}
		fmt.Printf("Wrote %s\n", destination)
		return
	}

	// hand the build version to the server package so GET /api/health can report it
	// to the frontend top bar.
	server.BuildVersion = version

	printBanner(*addr)

	// capture backend logs into the in-memory sink (still to stderr) so the /logs
	// page can show a live log stream. Do this first, to catch startup logs too.
	server.StartLogCapture()

	cfg, err := config.Read()
	if err != nil {
		log.Fatalf("[config] %v", err)
	}
	if cfg.Exists {
		log.Printf("[config] 配置文件: %s", cfg.Path)
		if cfg.Legacy {
			log.Print("[config] Legacy JSON configuration loaded. Use -migrate-config to create a TOML copy.")
		}
	}
	dsn, source, err := cfg.PostgresDSN()
	if err != nil {
		log.Fatalf("[config] %v", err)
	}
	log.Printf("[pg] 数据库配置来源: %s", source)

	sigCtx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ctx, shutdown := shutdownContext(sigCtx)
	defer shutdown(agent.AbortShutdown)

	mgr, err := server.OpenManager(*dataDir, *proxy, dsn)
	if err != nil {
		log.Fatalf("open stores: %v", err)
	}
	defer mgr.Close()

	skillDir := cfg.SkillDir()
	if abs, err := filepath.Abs(skillDir); err == nil {
		skillDir = abs
	}
	if err := os.MkdirAll(skillDir, 0o755); err != nil {
		log.Fatalf("[config] create skill directory: %v", err)
	}
	log.Printf("[config] skill 目录: %s", skillDir)
	srv := server.New(ctx, mgr, skillDir, *dataDir, config.BaseDir())
	httpSrv := &http.Server{
		Addr:              *addr,
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	go func() {
		log.Printf("ARTEX %s backend listening on %s (data=%s, workers=%d)", version, *addr, *dataDir, mgr.Workers())
		if err := httpSrv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("serve: %v", err)
		}
	}()

	<-ctx.Done()

	log.Println("shutting down...")
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_ = httpSrv.Shutdown(shutdownCtx)
}

// shutdownContext deliberately does not derive from signalCtx. If it did, the
// parent's plain context.Canceled could win the race before AbortShutdown was
// attached to the child, losing the diagnostic cause in every running Agent.
func shutdownContext(signalCtx context.Context) (context.Context, context.CancelCauseFunc) {
	ctx, shutdown := context.WithCancelCause(context.Background())
	go func() {
		select {
		case <-signalCtx.Done():
			shutdown(agent.AbortShutdown)
		case <-ctx.Done():
		}
	}()
	return ctx, shutdown
}
