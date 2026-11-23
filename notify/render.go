package notify

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"
)

const (
	telegramTextLimit = 4096
	maxAssetsShown    = 3
	maxSummaryRunes   = 120
	ellipsis          = "…"
)

// telegramHTML renders HTML and returns the number of items that fit.
func telegramHTML(m Message) (string, int) {
	if m.Batch {
		return telegramBatchHTML(m)
	}
	var b strings.Builder
	title := "漏洞通知"
	if len(m.Items) > 0 {
		item := m.Items[0]
		title = fmt.Sprintf("[%s] %s", severityLabel(item.Severity), oneLine(item.Title(), 0))
	}
	b.WriteString("<b>" + telegramEscape(title) + "</b>\n")
	if len(m.Items) == 0 {
		return b.String(), 0
	}
	it := m.Items[0]
	if it.IsStatusChange() {
		b.WriteString(fmt.Sprintf("\n<b>状态变更</b>：%s → %s",
			telegramEscape(statusLabel(it.FromStatus)), telegramEscape(statusLabel(it.ToStatus))))
	}
	if it.VulnClass != "" && it.VulnClass != it.Title() {
		b.WriteString("\n<b>类型</b>：" + telegramEscape(it.VulnClass))
	}
	if a := assetLine(it.Assets, maxAssetsShown); a != "" {
		b.WriteString("\n<b>资产</b>：" + telegramEscape(a))
	}
	if s := oneLine(it.Summary, maxSummaryRunes); s != "" {
		b.WriteString("\n<b>摘要</b>：" + telegramEscape(s))
	}
	if it.DetailURL != "" {
		b.WriteString(fmt.Sprintf("\n\n<a href=\"%s\">查看详情</a>", telegramEscapeAttr(it.DetailURL)))
	}
	return truncateHTML(b.String(), telegramTextLimit), 1
}

// telegramBatchHTML measures each escaped entry, including its line break.
// Count only rendered entries, so the outbox can safely acknowledge that prefix.
func telegramBatchHTML(m Message) (string, int) {
	footer := ""
	if m.HomeURL != "" {
		footer = fmt.Sprintf("\n\n<a href=\"%s\">在平台中查看全部</a>", telegramEscapeAttr(m.HomeURL))
	}
	footerRunes := utf8.RuneCountInString(footer)
	var lines strings.Builder
	kept := 0
	keptRunes := 0
	for i, item := range m.Items {
		line := "\n" + telegramEscape(telegramBatchLine(item, i+1))
		heading := telegramBatchHeading(m, i+1)
		lineRunes := utf8.RuneCountInString(line)
		size := utf8.RuneCountInString(heading) + lineRunes + footerRunes
		if keptRunes+size > telegramTextLimit {
			if i == 0 {
				// Send a shortened first item if it alone exceeds the budget.
				return truncateHTML(heading+line, telegramTextLimit), 1
			}
			break
		}
		lines.WriteString(line)
		keptRunes += lineRunes
		kept++
	}
	return truncateHTML(telegramBatchHeading(m, kept)+lines.String()+footer, telegramTextLimit), kept
}

func telegramBatchHeading(m Message, kept int) string {
	title := fmt.Sprintf("漏洞汇总 · 共 %d 条", len(m.Items))
	if extra := len(m.Items) - kept; extra > 0 {
		title += fmt.Sprintf("（显示前 %d 条，其余 %d 条下一条继续）", kept, extra)
	}
	if m.WindowMinutes > 0 {
		title = fmt.Sprintf("近 %d 分钟 · %s", m.WindowMinutes, title)
	}
	return "<b>" + telegramEscape(title) + "</b>"
}

// telegramBatchLine formats one batch item. The caller escapes the resulting line.
func telegramBatchLine(it Item, idx int) string {
	if a := assetLine(it.Assets, maxAssetsShown); a != "" {
		return fmt.Sprintf("%d. %s · %s — %s", idx, severityLabel(it.Severity), it.Title(), a)
	}
	return fmt.Sprintf("%d. %s · %s", idx, severityLabel(it.Severity), it.Title())
}

// telegramEscape encodes HTML text, including existing entities, so input is shown literally.
func telegramEscape(s string) string {
	s = strings.ReplaceAll(s, "&", "&amp;")
	s = strings.ReplaceAll(s, "<", "&lt;")
	s = strings.ReplaceAll(s, ">", "&gt;")
	return s
}

// telegramEscapeAttr also escapes quotes to keep attribute values within their delimiters.
func telegramEscapeAttr(s string) string {
	s = telegramEscape(s)
	s = strings.ReplaceAll(s, "\"", "&quot;")
	return s
}

// oneLine folds whitespace and truncates by rune count. A nonpositive max leaves length unrestricted.
func oneLine(s string, max int) string {
	s = strings.Join(strings.Fields(s), " ")
	return truncateRunes(s, max)
}

// truncateRunes limits s to max runes and adds an ellipsis when truncated. A nonpositive max means unlimited length.
func truncateRunes(s string, max int) string {
	if max <= 0 {
		return s
	}
	runes := []rune(s)
	if len(runes) <= max {
		return s
	}
	if max <= 1 {
		return string(runes[:max])
	}
	return string(runes[:max-1]) + ellipsis
}

// truncateHTML limits an HTML fragment by rune count without leaving a partial tag.
//
// Trim a trailing partial tag after truncation so the destination parser can read the fragment.
//
// Leave tag balancing to Telegram's HTML parser.
func truncateHTML(s string, max int) string {
	if max <= 0 || utf8.RuneCountInString(s) <= max {
		return s
	}
	cut := truncateRunes(s, max)
	// Trim back to the last opening angle bracket when the closing bracket was truncated.
	if lt := strings.LastIndex(cut, "<"); lt >= 0 && !strings.Contains(cut[lt:], ">") {
		cut = cut[:lt]
	}
	// Remove a partial HTML entity as well. A strict parser might reject the entire message otherwise.
	if amp := strings.LastIndex(cut, "&"); amp >= 0 && !strings.Contains(cut[amp:], ";") {
		cut = cut[:amp]
	}
	return cut
}

// assetLine formats up to limit assets and shows the total when names are omitted.
func assetLine(assets []string, limit int) string {
	if len(assets) == 0 {
		return ""
	}
	if limit <= 0 || len(assets) <= limit {
		return strings.Join(assets, "、")
	}
	return strings.Join(assets[:limit], "、") + " 等 " + strconv.Itoa(len(assets)) + " 个"
}

// severityLabel returns a localized severity label with an emoji. Unknown labels pass through unchanged.
func severityLabel(severity string) string {
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

// statusLabel translates a finding status for a status-change message.
func statusLabel(status string) string {
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
