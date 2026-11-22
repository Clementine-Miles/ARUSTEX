package report

import (
	"bytes"
	"encoding/csv"
	"fmt"
	"path"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/Autumn-27/artex/db"
)

// Render findings for export as summary Markdown, individual Markdown, or CSV. The server serializes JSON separately.

// sortFindingsForExport sorts by descending severity and newest first.
func sortFindingsForExport(fs []*db.DBFinding) {
	sort.SliceStable(fs, func(i, j int) bool {
		ri, rj := sevRank[fs[i].Severity], sevRank[fs[j].Severity]
		if ri != rj {
			return ri < rj // smaller sevRank means higher severity
		}
		return fs[i].CreatedAt.After(fs[j].CreatedAt)
	})
}

// findingTitle prefers the name, then category, then an uncategorized label.
func findingTitle(f *db.DBFinding) string {
	return nz(f.Name, nz(f.VulnClass, "未分类"))
}

// FindingsMarkdown renders a summary and groups findings by severity with status, task, evidence, and report details.
func FindingsMarkdown(fs []*db.DBFinding, generatedAt time.Time) string {
	items := append([]*db.DBFinding(nil), fs...)
	sortFindingsForExport(items)

	var b strings.Builder
	b.WriteString("# 漏洞发现汇总报告\n\n")
	fmt.Fprintf(&b, "- **生成时间**：%s\n", generatedAt.Format("2006-01-02 15:04:05"))
	fmt.Fprintf(&b, "- **发现总数**：%d 个\n\n", len(items))

	// Count findings by severity for the summary.
	counts := map[string]int{}
	for _, f := range items {
		counts[f.Severity]++
	}
	b.WriteString("## 摘要\n\n")
	b.WriteString("| 严重等级 | 数量 |\n| --- | --- |\n")
	for _, s := range []struct{ key, label string }{
		{"critical", "严重"}, {"high", "高危"}, {"medium", "中危"}, {"low", "低危"},
	} {
		fmt.Fprintf(&b, "| %s | %d |\n", s.label, counts[s.key])
	}
	b.WriteString("\n")

	if len(items) == 0 {
		b.WriteString("_无匹配的漏洞。_\n")
		return b.String()
	}

	b.WriteString("## 漏洞明细\n\n")
	for i, f := range items {
		fmt.Fprintf(&b, "### %d. [%s] %s\n\n", i+1, strings.ToUpper(nz(f.Severity, "info")), findingTitle(f))
		if f.VulnClass != "" {
			fmt.Fprintf(&b, "- **类别**：%s\n", f.VulnClass)
		}
		fmt.Fprintf(&b, "- **状态**：%s\n", nz(f.Status, "pending"))
		if desc := strings.TrimSpace(f.TaskDescription); desc != "" {
			fmt.Fprintf(&b, "- **所属任务**：%s\n", desc)
		}
		fmt.Fprintf(&b, "- **发现时间**：%s\n\n", f.CreatedAt.Format("2006-01-02 15:04:05"))
		if s := strings.TrimSpace(f.Summary); s != "" {
			fmt.Fprintf(&b, "%s\n\n", s)
		}
		if e := strings.TrimSpace(f.Evidence); e != "" {
			fmt.Fprintf(&b, "**证据：**\n\n```\n%s\n```\n\n", e)
		}
		if rep := strings.TrimSpace(f.Report); rep != "" {
			b.WriteString("**详细报告：**\n\n")
			b.WriteString(rep)
			b.WriteString("\n\n")
		}
		b.WriteString(findingTrafficMarkdown(f, false))
		b.WriteString("---\n\n")
	}
	return b.String()
}

// SingleFindingMarkdown renders one finding as a standalone Markdown file.
func SingleFindingMarkdown(f *db.DBFinding, generatedAt time.Time) string {
	var b strings.Builder
	fmt.Fprintf(&b, "# [%s] %s\n\n", strings.ToUpper(nz(f.Severity, "info")), findingTitle(f))
	if f.VulnClass != "" {
		fmt.Fprintf(&b, "- **类别**：%s\n", f.VulnClass)
	}
	fmt.Fprintf(&b, "- **严重等级**：%s\n", nz(f.Severity, "info"))
	fmt.Fprintf(&b, "- **状态**：%s\n", nz(f.Status, "pending"))
	if desc := strings.TrimSpace(f.TaskDescription); desc != "" {
		fmt.Fprintf(&b, "- **所属任务**：%s\n", desc)
	}
	fmt.Fprintf(&b, "- **发现时间**：%s\n", f.CreatedAt.Format("2006-01-02 15:04:05"))
	fmt.Fprintf(&b, "- **生成时间**：%s\n\n", generatedAt.Format("2006-01-02 15:04:05"))
	if s := strings.TrimSpace(f.Summary); s != "" {
		fmt.Fprintf(&b, "## 概述\n\n%s\n\n", s)
	}
	if e := strings.TrimSpace(f.Evidence); e != "" {
		fmt.Fprintf(&b, "## 证据\n\n```\n%s\n```\n\n", e)
	}
	if rep := strings.TrimSpace(f.Report); rep != "" {
		b.WriteString("## 详细报告\n\n")
		b.WriteString(rep)
		b.WriteString("\n")
	}
	b.WriteString(findingTrafficMarkdown(f, true))
	return b.String()
}

var unsafeFilenameChars = regexp.MustCompile(`[^\p{Han}\p{L}\p{N}._-]+`)

// FindingFilename builds a safe Markdown archive name such as critical_sql-injection_#123.md by removing path separators and control characters.
func FindingFilename(f *db.DBFinding) string {
	sev := nz(f.Severity, "info")
	title := findingTitle(f)
	name := fmt.Sprintf("%s_%s_#%d", sev, title, f.ID)
	name = unsafeFilenameChars.ReplaceAllString(name, "_")
	name = strings.Trim(name, "._")
	if name == "" {
		name = fmt.Sprintf("finding_%d", f.ID)
	}
	// Remove any remaining path components before archiving.
	name = path.Base(name)
	if len(name) > 120 {
		name = name[:120]
	}
	return name + ".md"
}

// FindingsCSV writes UTF-8 with a BOM for spreadsheet compatibility. Use Markdown or JSON for full reports and evidence.
func FindingsCSV(fs []*db.DBFinding) []byte {
	items := append([]*db.DBFinding(nil), fs...)
	sortFindingsForExport(items)

	var buf bytes.Buffer
	buf.WriteString("\xEF\xBB\xBF") // UTF-8 BOM
	w := csv.NewWriter(&buf)
	_ = w.Write([]string{"ID", "名称", "类别", "严重等级", "状态", "所属任务", "发现时间", "概述", "流量证据数量", "流量证据ID"})
	for _, f := range items {
		_ = w.Write([]string{
			fmt.Sprintf("%d", f.ID),
			findingTitle(f),
			f.VulnClass,
			nz(f.Severity, "info"),
			nz(f.Status, "pending"),
			f.TaskDescription,
			f.CreatedAt.Format("2006-01-02 15:04:05"),
			strings.TrimSpace(f.Summary),
			fmt.Sprint(len(f.TrafficBindings)), findingTrafficIDs(f),
		})
	}
	w.Flush()
	return buf.Bytes()
}

func findingTrafficIDs(f *db.DBFinding) string {
	ids := make([]string, 0, len(f.TrafficBindings))
	for _, b := range f.TrafficBindings {
		ids = append(ids, fmt.Sprint(b.ID))
	}
	return strings.Join(ids, ",")
}

func findingTrafficMarkdown(f *db.DBFinding, attachments bool) string {
	stale := f.Report != "" && f.EvidenceVersion != f.ReportEvidenceVersion
	if len(f.TrafficBindings) == 0 && !stale {
		return ""
	}
	var out strings.Builder
	out.WriteString("\n## 关联流量证据\n\n")
	fmt.Fprintf(&out, "证据版本：%d；绑定数量：%d。\n\n", f.EvidenceVersion, len(f.TrafficBindings))
	if stale {
		out.WriteString("证据已变更，详细报告待更新。\n\n")
	}
	for i, b := range f.TrafficBindings {
		fmt.Fprintf(&out, "%d. **证据 #%d · %s** — `%s %s`，状态码 %d\n", i+1, b.ID, b.Role, b.Snapshot.Method, strings.ReplaceAll(b.Snapshot.URL, "`", "%60"), b.Snapshot.Status)
		if b.Note != "" {
			fmt.Fprintf(&out, "   %s\n", strings.ReplaceAll(b.Note, "\n", "\n   "))
		}
		if attachments {
			fmt.Fprintf(&out, "   [请求报文](evidence/%d/%d/request.http) · [响应报文](evidence/%d/%d/response.http)\n", f.ID, b.ID, f.ID, b.ID)
		}
	}
	out.WriteString("\n")
	return out.String()
}
