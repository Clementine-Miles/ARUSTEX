package notify

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"strings"
)

// telegramTextLimit bounds the sendMessage text field in characters.
const telegramTextLimit = 4096

// telegramChannel implements the Telegram Bot API.
//
// Telegram authenticates with a path token. HTML mode uses a smaller escaping set than MarkdownV2. Check the body ok flag even on HTTP 200.
type telegramChannel struct{}

func (telegramChannel) Kind() string { return KindTelegram }

// Use a conservative limit for both direct and group chats.
func (telegramChannel) DefaultRatePerMin() int { return 20 }

// The bot token is a credential. chat_id identifies a recipient.
func (telegramChannel) SecretKeys() []string { return []string{"bot_token"} }

// Changing base_url requires an explicit token because it selects the endpoint that receives the credential.
func (telegramChannel) DestinationKeys() []string { return []string{"base_url"} }

func (telegramChannel) Validate(cfg map[string]any) error {
	if cfgString(cfg, "bot_token") == "" {
		return errors.New("缺少 Bot Token")
	}
	if cfgString(cfg, "chat_id") == "" {
		return errors.New("缺少 Chat ID")
	}
	if base := cfgString(cfg, "base_url"); base != "" {
		if err := validateHTTPURL(base); err != nil {
			return fmt.Errorf("API 地址无效: %w", err)
		}
	}
	return nil
}

func (c telegramChannel) Send(ctx context.Context, cfg map[string]any, m Message) (int, error) {
	if err := c.Validate(cfg); err != nil {
		return 0, Permanent(err)
	}
	endpoint, err := telegramEndpoint(cfg)
	if err != nil {
		return 0, Permanent(err)
	}
	text, kept := telegramHTML(m)
	payload := map[string]any{
		"chat_id":                  cfgString(cfg, "chat_id"),
		"text":                     text,
		"parse_mode":               "HTML",
		"disable_web_page_preview": false,
	}
	raw, err := doJSON(ctx, "POST", endpoint, nil, payload)
	if err != nil {
		return 0, err
	}
	var res struct {
		OK          bool   `json:"ok"`
		ErrorCode   int    `json:"error_code"`
		Description string `json:"description"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return 0, fmt.Errorf("解析 Telegram 响应失败: %w (%s)", err, snippet(raw))
	}
	if res.OK {
		return kept, nil
	}
	// Retry rate limits with backoff. Other listed API errors signal configuration problems.
	if res.ErrorCode == 429 {
		return 0, fmt.Errorf("Telegram 限流: %s", res.Description)
	}
	return 0, Permanent(fmt.Errorf("Telegram 返回错误 %d: %s", res.ErrorCode, res.Description))
}

// telegramEndpoint uses the official API by default and accepts a custom Bot API relay.
func telegramEndpoint(cfg map[string]any) (string, error) {
	base := cfgString(cfg, "base_url")
	if base == "" {
		base = "https://api.telegram.org"
	}
	base = strings.TrimSuffix(base, "/")
	token := cfgString(cfg, "bot_token")
	raw := base + "/bot" + token + "/sendMessage"
	u, err := url.Parse(raw)
	if err != nil {
		// Suppress parse errors that could include the bot token.
		return "", fmt.Errorf("拼接 API 地址失败（API 地址：%s）", redactRequestTarget(base))
	}
	return u.String(), nil
}

// telegramHTML renders HTML and returns the number of items included.
func telegramHTML(m Message) (string, int) {
	var b strings.Builder
	b.WriteString("<b>" + telegramEscape(markdownTitle(m)) + "</b>\n")
	if m.Batch {
		// Telegram measures the batch budget in characters.
		footer := ""
		if m.HomeURL != "" {
			footer = fmt.Sprintf("\n\n<a href=\"%s\">在平台中查看全部</a>", telegramEscapeAttr(m.HomeURL))
		}
		kept := packItemCount(m.Items, telegramTextLimit, telegramReservedRunes, footer, runeSize, func(it Item, idx int) string {
			return telegramBatchLine(it, idx+1)
		})
		items := m.Items[:kept]
		b.Reset()
		b.WriteString("<b>" + telegramEscape(telegramBatchTitle(m, items, len(m.Items))) + "</b>")
		for i, it := range items {
			b.WriteString("\n" + telegramEscape(telegramBatchLine(it, i+1)))
		}
		b.WriteString(footer)
		return TruncateHTML(b.String(), telegramTextLimit), kept
	}
	if len(m.Items) == 0 {
		return b.String(), 0
	}
	it := m.Items[0]
	if it.IsStatusChange() {
		b.WriteString(fmt.Sprintf("\n<b>状态变更</b>：%s → %s",
			telegramEscape(StatusLabel(it.FromStatus)), telegramEscape(StatusLabel(it.ToStatus))))
	}
	if it.VulnClass != "" && it.VulnClass != it.Title() {
		b.WriteString("\n<b>类型</b>：" + telegramEscape(it.VulnClass))
	}
	if a := assetLine(it.Assets, maxAssetsShown); a != "" {
		b.WriteString("\n<b>资产</b>：" + telegramEscape(a))
	}
	if s := OneLine(it.Summary, maxSummaryRunes); s != "" {
		b.WriteString("\n<b>摘要</b>：" + telegramEscape(s))
	}
	if it.DetailURL != "" {
		b.WriteString(fmt.Sprintf("\n\n<a href=\"%s\">查看详情</a>", telegramEscapeAttr(it.DetailURL)))
	}
	return TruncateHTML(b.String(), telegramTextLimit), 1
}

// telegramReservedRunes reserves space for the headline and truncation notice.
const telegramReservedRunes = 160

// telegramBatchLine formats one batch item. The caller escapes the resulting line.
func telegramBatchLine(it Item, idx int) string {
	if a := assetLine(it.Assets, maxAssetsShown); a != "" {
		return fmt.Sprintf("%d. %s · %s — %s", idx, SeverityLabel(it.Severity), it.Title(), a)
	}
	return fmt.Sprintf("%d. %s · %s", idx, SeverityLabel(it.Severity), it.Title())
}

// telegramBatchTitle uses the number of items in the current message.
func telegramBatchTitle(m Message, items []Item, total int) string {
	title := fmt.Sprintf("漏洞汇总 · 共 %d 条", total)
	if extra := total - len(items); extra > 0 {
		title += fmt.Sprintf("（显示前 %d 条，其余 %d 条下一条继续）", len(items), extra)
	}
	if m.WindowMinutes > 0 {
		title = fmt.Sprintf("近 %d 分钟 · %s", m.WindowMinutes, title)
	}
	return title
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
