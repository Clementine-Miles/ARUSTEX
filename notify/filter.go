package notify

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
)

// Filter is the notification_channels.filter JSONB configuration. Omitted
// criteria impose no restriction, and status changes require explicit opt-in.
type Filter struct {
	// MinSeverity is a low, medium, high, or critical threshold. Empty means no threshold.
	MinSeverity string `json:"min_severity"`
	// An empty TaskIDs or AssetIDs list imposes no restriction. Nonempty lists
	// must overlap the corresponding event values.
	TaskIDs  []int64 `json:"task_ids"`
	AssetIDs []int64 `json:"asset_ids"`
	// Include accepts a matching finding class, while any exclude hit wins.
	// Matching uses case-insensitive substrings.
	VulnClassInclude []string `json:"vulnclass_include"`
	VulnClassExclude []string `json:"vulnclass_exclude"`
	// OnStatusChange enables delivery of status events in real-time mode.
	OnStatusChange bool `json:"on_status_change"`
}

// ParseFilter reads stored criteria without failing on decoding errors.
// Fields that decode successfully may still apply to legacy configurations.
func ParseFilter(raw []byte) Filter {
	var f Filter
	if len(raw) == 0 {
		return f
	}
	_ = json.Unmarshal(raw, &f)
	return f
}

// ValidMinSeverity accepts known thresholds and the empty default.
func ValidMinSeverity(s string) bool {
	if s == "" {
		return true
	}
	_, ok := severityRank[s]
	return ok
}

// Validate checks constrained fields before persisting the configuration.
//
// An unknown threshold ranks as zero. Reject it at save time because the
// runtime fallback would deliver every severity.
//
// Apply strict validation on writes. Reads still accept legacy stored values.
func (f Filter) Validate() error {
	if !ValidMinSeverity(f.MinSeverity) {
		return fmt.Errorf("最低级别 %q 无效，可选：low / medium / high / critical，或留空表示不限", f.MinSeverity)
	}
	return nil
}

// Match reports whether an event should be delivered to this channel.
//
// Check event kind, severity, task and asset membership, and class keywords.
func Match(f Filter, s Snapshot) bool {
	// Status transitions are delivered only when enabled. New findings still use
	// the normal filter.
	if s.Kind == EventFindingStatusChanged && !f.OnStatusChange {
		return false
	}
	if !AtLeast(s.Severity, f.MinSeverity) {
		return false
	}
	if len(f.TaskIDs) > 0 && !slices.Contains(f.TaskIDs, s.TaskID) {
		return false
	}
	if len(f.AssetIDs) > 0 && !intersectsInt(f.AssetIDs, s.AssetIDs) {
		return false
	}
	// Any exclusion takes precedence over inclusion.
	if len(f.VulnClassExclude) > 0 && containsAnyFold(s.VulnClass, f.VulnClassExclude) {
		return false
	}
	if len(f.VulnClassInclude) > 0 && !containsAnyFold(s.VulnClass, f.VulnClassInclude) {
		return false
	}
	return true
}

func intersectsInt(a, b []int64) bool {
	// Small operator-selected lists are cheap enough to scan directly.
	for _, v := range b {
		if slices.Contains(a, v) {
			return true
		}
	}
	return false
}

// containsAnyFold reports whether s contains a keyword, ignoring case.
func containsAnyFold(s string, keywords []string) bool {
	lower := strings.ToLower(s)
	for _, kw := range keywords {
		kw = strings.ToLower(strings.TrimSpace(kw))
		if kw != "" && strings.Contains(lower, kw) {
			return true
		}
	}
	return false
}

// severityRank orders finding severities. An unknown severity ranks below known values and is filtered when a minimum is set.
var severityRank = map[string]int{
	"low":      1,
	"medium":   2,
	"high":     3,
	"critical": 4,
}

// SeverityRank returns the severity rank, or zero when unknown.
func SeverityRank(severity string) int { return severityRank[severity] }

// AtLeast compares severity with min. An empty minimum accepts any severity, and a nonempty minimum rejects unknown severities.
func AtLeast(severity, min string) bool {
	if min == "" {
		return true
	}
	return SeverityRank(severity) >= SeverityRank(min)
}
