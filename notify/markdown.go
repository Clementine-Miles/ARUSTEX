package notify

import (
	"fmt"
	"strings"
)

// DingTalk and WeCom share Markdown rendering. The other channels render their own card or HTML formats.

// maxAssetsShown limits the number of assets listed for each finding.
const maxAssetsShown = 3

// maxSummaryRunes bounds the summary length. The full report is available through the details link.
const maxSummaryRunes = 120

// markdownReservedBytes reserves room for the batch heading, severity counts, truncation notice, and footer link.
const markdownReservedBytes = 320

// markdownEscape escapes Markdown syntax.
//
// Finding text and scanned asset URLs are untrusted, including query parameters.
//
//
// Escaping prevents untrusted text from injecting links, external images, or structural markup into notifications.
//
// Escape backslashes first so the escapes added for other Markdown syntax stay intact.
func markdownEscape(s string) string {
	replacer := strings.NewReplacer(
		`\`, `\\`,
		"`", "\\`",
		"*", `\*`,
		"_", `\_`,
		"[", `\[`,
		"]", `\]`,
		"(", `\(`,
		")", `\)`,
		"!", `\!`,
		"#", `\#`,
		">", `\>`,
		"|", `\|`,
		"~", `\~`,
	)
	return replacer.Replace(s)
}

// markdownText folds and escapes untrusted text. Folding line breaks prevents new list items and blockquotes.
func markdownText(s string, maxRunes int) string {
	return markdownEscape(OneLine(s, maxRunes))
}

// markdownTitle returns the original unescaped message title.
//
// Each renderer escapes the title for its own format. Markdown escapes would otherwise appear literally in Telegram HTML or webhook JSON.
func markdownTitle(m Message) string {
	if m.Batch {
		return fmt.Sprintf("漏洞汇总 · 共 %d 条", len(m.Items))
	}
	if len(m.Items) == 0 {
		return "漏洞通知"
	}
	it := m.Items[0]
	return fmt.Sprintf("[%s] %s", SeverityLabel(it.Severity), OneLine(it.Title(), 0))
}

// markdownBody renders the message and returns the number of items included.
//
// Mark only the first kept items as delivered. Items beyond the channel's size limit remain queued for the next batch.
//
// A nonpositive maxBytes means unlimited length.
func markdownBody(m Message, maxBytes int) (string, int) {
	if !m.Batch {
		if len(m.Items) == 0 {
			return "", 0
		}
		var b strings.Builder
		writeItem(&b, m.Items[0], "", true)
		// Send a single oversized item and apply the final truncation safeguard so the queue can advance.
		return TruncateBytes(b.String(), maxBytes), 1
	}

	footer := ""
	if m.HomeURL != "" {
		footer = fmt.Sprintf("\n[在平台中查看全部](%s)\n", m.HomeURL)
	}
	kept := packItemCount(m.Items, maxBytes, markdownReservedBytes, footer, byteSize, func(it Item, idx int) string {
		var b strings.Builder
		writeItem(&b, it, fmt.Sprintf("%d. ", idx+1), false)
		return b.String()
	})

	items := m.Items[:kept]
	var b strings.Builder
	b.WriteString(markdownBatchIntro(m, items, len(m.Items)))
	for i, it := range items {
		writeItem(&b, it, fmt.Sprintf("%d. ", i+1), false)
	}
	b.WriteString(footer)
	return TruncateBytes(b.String(), maxBytes), kept
}

// markdownBatchIntro renders the batch time window, item count, and severity distribution.
//
// Report how many of the total items fit and how many remain for later delivery.
func markdownBatchIntro(m Message, items []Item, total int) string {
	var b strings.Builder
	if m.WindowMinutes > 0 {
		fmt.Fprintf(&b, "**近 %d 分钟新增 %d 个漏洞**", m.WindowMinutes, total)
	} else {
		fmt.Fprintf(&b, "**新增 %d 个漏洞**", total)
	}
	if extra := total - len(items); extra > 0 {
		fmt.Fprintf(&b, "（本条显示前 %d 条，其余 %d 条将在下一条消息继续）", len(items), extra)
	}
	// Count only the items in this message so the severity totals match the list.
	counts := map[string]int{}
	for _, it := range items {
		counts[it.Severity]++
	}
	var parts []string
	for _, sev := range []string{"critical", "high", "medium", "low"} {
		if n := counts[sev]; n > 0 {
			parts = append(parts, fmt.Sprintf("%s %d", SeverityLabel(sev), n))
		}
	}
	if len(parts) > 0 {
		b.WriteString("\n" + strings.Join(parts, " · "))
	}
	b.WriteString("\n\n")
	return b.String()
}

// writeItem renders one finding.
//
// prefix numbers batch entries. Single-item messages include the full summary and detail link, while batch entries use one line.
//
// Escape and fold externally supplied text. The administrator-configured detail URL is rendered as a clickable link.
func writeItem(b *strings.Builder, it Item, prefix string, single bool) {
	line := fmt.Sprintf("%s**%s · %s**", prefix, SeverityLabel(it.Severity), markdownText(it.Title(), 0))
	if !single {
		// The batch format keeps assets and summary on a single line.
		var extras []string
		if a := assetLine(it.Assets, maxAssetsShown); a != "" {
			extras = append(extras, markdownText(a, 0))
		}
		if it.Summary != "" {
			extras = append(extras, markdownText(it.Summary, 60))
		}
		if len(extras) > 0 {
			line += " — " + strings.Join(extras, " · ")
		}
		b.WriteString(line + "\n")
		return
	}
	b.WriteString(line + "\n")
	if it.IsStatusChange() {
		fmt.Fprintf(b, "**状态变更**：%s → %s\n",
			markdownText(StatusLabel(it.FromStatus), 0), markdownText(StatusLabel(it.ToStatus), 0))
	}
	if it.VulnClass != "" && it.VulnClass != it.Title() {
		fmt.Fprintf(b, "**类型**：%s\n", markdownText(it.VulnClass, 0))
	}
	if a := assetLine(it.Assets, maxAssetsShown); a != "" {
		fmt.Fprintf(b, "**资产**：%s\n", markdownText(a, 0))
	}
	if it.Summary != "" {
		if s := markdownText(it.Summary, maxSummaryRunes); s != "" {
			fmt.Fprintf(b, "**摘要**：%s\n", s)
		}
	}
	if it.DetailURL != "" {
		fmt.Fprintf(b, "[查看详情](%s)\n", it.DetailURL)
	}
}
