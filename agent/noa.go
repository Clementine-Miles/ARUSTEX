package agent

import (
	"log"
	"path/filepath"

	"github.com/Autumn-27/norma/agentcore"
	"github.com/Autumn-27/norma/noaadapter"
)

// noaWarn returns a diagnostics sink tagging non-fatal noa messages with the
// session, routed through the package logger (agents have no per-instance one).
func noaWarn(session string) func(string) {
	return func(msg string) { log.Printf("[noa] %s: %s", session, msg) }
}

// noa is the experimental model-driven compactor introduced in Norma v0.4.0. Enable installs its
// compactor, tool, and prompts in place of built-in compaction. Resolve the setting once per run
// so a toggle affects later runs without rebuilding the Agent.

// enableNoa attaches noa when enabled. Use the global workspace for a shared archive root and
// its unique session ID for the archive subdirectory.
//
// Warn and retain built-in compaction if noa initialization fails. Clear opts.Compaction on
// success to keep only one context manager.
func enableNoa(opts *agentcore.Options, enabled func() bool, archiveRoot, sessionID string, onWarn func(string)) {
	if enabled == nil || !enabled() {
		return
	}
	if opts.OnWarn == nil {
		opts.OnWarn = onWarn
	}
	if err := noaadapter.Enable(opts, noaadapter.Options{
		ArchiveBaseDir: filepath.Join(archiveRoot, "noa"),
		SessionID:      sessionID,
		OnWarn:         onWarn,
	}); err != nil {
		if onWarn != nil {
			onWarn("noa 压缩启用失败,回退内置压缩:" + err.Error())
		}
		return
	}
	// Compactor takes precedence. Clear Compaction to avoid SDK warnings about two context managers.
	opts.Compaction = nil
}
