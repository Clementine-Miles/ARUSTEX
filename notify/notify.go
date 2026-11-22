// Package notify implements finding notifications for messaging and email channels.
//
// This package depends only on the standard library. The server orchestrates delivery and passes JSONB-backed configuration as a map and the notification as a Message.
//
// Channel implementations are stateless and shared by concurrent deliveries. Read credentials from cfg on every call.
package notify

// Supported channel kinds are validated by the server so adding a channel does not require a database constraint migration.
const (
	KindDingTalk = "dingtalk" // DingTalk custom robot
	KindFeishu   = "feishu"   // Feishu and Lark custom robot
	KindWeCom    = "wecom"    // WeCom group robot
	KindWebhook  = "webhook"  // Generic webhook with configurable method, headers, and JSON template
	KindTelegram = "telegram" // Telegram Bot API
	KindEmail    = "email"    // SMTP email
)

// Event types correspond to notification_events.kind.
const (
	EventFindingCreated       = "finding_created"
	EventFindingStatusChanged = "finding_status_changed"
)

// InitKind supplies the fallback for an empty channel kind.
const InitKind = KindDingTalk

// severityRank orders finding severities. An unknown severity ranks below known values and is filtered when a minimum is set.
var severityRank = map[string]int{
	"low":      1,
	"medium":   2,
	"high":     3,
	"critical": 4,
}

// SeverityRank returns the severity rank, or zero when unknown.
func SeverityRank(severity string) int { return severityRank[severity] }

// SeverityLabel returns a localized severity label with an emoji. Unknown labels pass through unchanged.
func SeverityLabel(severity string) string {
	switch severity {
	case "critical":
		return "🔴 严重"
	case "high":
		return "🟠 高危"
	case "medium":
		return "🟡 中危"
	case "low":
		return "🔵 低危"
	default:
		return severity
	}
}

// StatusLabel translates a finding status for a status-change message.
func StatusLabel(status string) string {
	switch status {
	case "pending":
		return "待处理"
	case "in_progress":
		return "处理中"
	case "confirmed":
		return "已确认"
	case "resolved":
		return "已处理"
	case "fixed":
		return "已修复"
	case "false_positive":
		return "误报"
	case "ignored":
		return "忽略"
	case "duplicate":
		return "重复"
	case "risk_accepted":
		return "风险接受"
	default:
		return status
	}
}

// AtLeast compares severity with min. An empty minimum accepts any severity, and a nonempty minimum rejects unknown severities.
func AtLeast(severity, min string) bool {
	if min == "" {
		return true
	}
	return SeverityRank(severity) >= SeverityRank(min)
}
