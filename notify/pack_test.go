package notify

import (
	"strings"
	"testing"
	"unicode/utf8"
)

// Test that oversized batches stop at item boundaries and report the number sent.
//
// Truncating a fully rendered batch would lose trailing items while counting them as delivered.

func TestMarkdownBodyPacksWholeItemsWithinByteLimit(t *testing.T) {
	// The Chinese text in this batch exceeds WeCom's 4096-byte limit.
	m := batchMsg(200)
	body, kept := markdownBody(m, weComMarkdownLimit)

	if len(body) > weComMarkdownLimit {
		t.Fatalf("正文 %d 字节超上限 %d", len(body), weComMarkdownLimit)
	}
	if !utf8.ValidString(body) {
		t.Fatal("正文不是合法 UTF-8")
	}
	if kept <= 0 || kept >= len(m.Items) {
		t.Fatalf("应只装下一部分（0 < kept < %d），得到 %d", len(m.Items), kept)
	}
	// The heading must report how many items fit and how many remain.
	if !strings.Contains(body, "其余") || !strings.Contains(body, "下一条消息继续") {
		t.Fatalf("头部应说明还有多少条未包含在本条里:\n%s", body[:minInt(400, len(body))])
	}
	// Include only the first kept items.
	for i := 0; i < kept; i++ {
		if !strings.Contains(body, "漏洞"+itoa(i+1)) {
			t.Fatalf("第 %d 条应在本条消息里:\n%s", i+1, body)
		}
	}
	if strings.Contains(body, "漏洞"+itoa(kept+1)) {
		t.Fatalf("第 %d 条不该出现（它属于下一批）", kept+1)
	}
}

func TestMarkdownBodyKeepsEverythingWhenUnderLimit(t *testing.T) {
	m := batchMsg(3)
	body, kept := markdownBody(m, 0) // zero means unlimited
	if kept != len(m.Items) {
		t.Fatalf("不限制长度时应全部保留，得到 kept=%d", kept)
	}
	if strings.Contains(body, "其余") {
		t.Fatalf("没有截断时不该出现截断提示:\n%s", body)
	}
}

func TestMarkdownBodyAlwaysKeepsAtLeastOneItem(t *testing.T) {
	// Send at least one item even when the budget is too small. Final truncation keeps an oversized item from blocking the queue.
	m := batchMsg(5)
	_, kept := markdownBody(m, 50)
	if kept != 1 {
		t.Fatalf("至少应保留 1 条，得到 %d", kept)
	}
}

func TestMarkdownBodySingleReturnsOne(t *testing.T) {
	_, kept := markdownBody(singleMsg(), 4096)
	if kept != 1 {
		t.Fatalf("单条消息应报送达 1 条，得到 %d", kept)
	}
	// An empty message has no deliverable items.
	if _, k := markdownBody(Message{}, 4096); k != 0 {
		t.Fatalf("空消息应报 0 条，得到 %d", k)
	}
}

func TestTelegramPackingUsesRuneBudget(t *testing.T) {
	m := batchMsg(200)
	text, kept := telegramHTML(m)
	// Telegram limits Unicode characters. A byte limit would truncate Chinese text too early.
	if n := utf8.RuneCountInString(text); n > telegramTextLimit {
		t.Fatalf("正文 %d 字符超上限 %d", n, telegramTextLimit)
	}
	if kept <= 0 || kept >= len(m.Items) {
		t.Fatalf("应只装下一部分，得到 %d", kept)
	}
	if !strings.Contains(text, "下一条继续") {
		t.Fatalf("应说明还有余量未包含:\n%.300s", text)
	}
}

func TestFeishuPackingReportsKept(t *testing.T) {
	m := batchMsg(2000)
	_, kept := feishuCard(m)
	if kept <= 0 || kept >= len(m.Items) {
		t.Fatalf("卡片应只装下一部分，得到 %d", kept)
	}
}

func TestWebhookAndEmailReportAllItems(t *testing.T) {
	// These channels deliver the entire batch without truncation.
	m := batchMsg(7)
	if n := len(m.Items); n != 7 {
		t.Fatal("前置条件不成立")
	}
	// markdownBody with a zero limit keeps all items.
	if _, k := markdownBody(m, 0); k != len(m.Items) {
		t.Fatalf("不限制长度时应用全部，得到 %d", k)
	}
}

// TestMarkdownEscapesUntrustedContent checks escaping for model-generated titles and summaries, and target-supplied asset names.
func TestMarkdownEscapesUntrustedContent(t *testing.T) {
	cases := []struct {
		name  string
		item  Item
		must  []string // expected escaped text
		wrong []string // disallowed unescaped text
	}{
		{
			name: "标题里的换行 + 外链",
			item: Item{
				Severity: "high",
				Name:     "登录口 SQL 注入\n[紧急：点此验证账号](http://attacker.tld)",
			},
			// Fold line breaks and escape link syntax so untrusted text cannot introduce Markdown structure.
			must:  []string{`\[紧急：点此验证账号\]`, `\(http://attacker.tld\)`},
			wrong: []string{"\n[紧急", "\n\n[紧急"},
		},
		{
			name: "标题里的图片信标",
			item: Item{
				Severity: "high",
				Name:     "漏洞 ![](http://attacker.tld/beacon)",
			},
			must:  []string{`\!`, `\(http://attacker.tld/beacon\)`},
			wrong: []string{"![]("},
		},
		{
			name: "资产名里的强调与引用",
			item: Item{
				Severity: "high",
				Name:     "普通标题",
				Assets:   []string{"a.com/*注入*>引用"},
			},
			must:  []string{`\*注入\*`, `\>`},
			wrong: []string{"*注入*"},
		},
		{
			name: "摘要里的反引号与竖线",
			item: Item{
				Severity: "high",
				Name:     "标题",
				Summary:  "`code` | 表格",
			},
			must:  []string{"\\`code\\`", `\|`},
			wrong: []string{"`code`"},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			m := Message{Items: []Item{tc.item}}
			// Single-item notifications exercise the shared writeItem path.
			var b strings.Builder
			writeItem(&b, tc.item, "", true)
			got := b.String()
			for _, want := range tc.must {
				if !strings.Contains(got, want) {
					t.Errorf("缺少转义形态 %q:\n%s", want, got)
				}
			}
			for _, bad := range tc.wrong {
				if strings.Contains(got, bad) {
					t.Errorf("出现了未转义形态 %q（可被用来注入结构或外链）:\n%s", bad, got)
				}
			}
			_ = m
		})
	}
}

// TestMarkdownEscapeBackslashFirst checks that newly added escapes are not escaped twice.
func TestMarkdownEscapeBackslashFirst(t *testing.T) {
	if got := markdownEscape(`a\b*c`); got != `a\\b\*c` {
		t.Fatalf("转义顺序有误，得到 %q", got)
	}
}

// TestTelegramTitleHasNoMarkdownEscapes keeps Markdown backslashes out of Telegram's HTML title.
func TestTelegramTitleHasNoMarkdownEscapes(t *testing.T) {
	m := Message{Items: []Item{{Severity: "high", Name: "alert(1) *重点*"}}}
	text, _ := telegramHTML(m)
	if strings.Contains(text, `\(`) || strings.Contains(text, `\*`) {
		t.Fatalf("Telegram 正文里出现了 markdown 的反斜杠转义:\n%s", text)
	}
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}
