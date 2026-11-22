package notify

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
)

// weComMarkdownLimit is WeCom's hard Markdown body limit in bytes.
const weComMarkdownLimit = 4096

// weComChannel implements WeCom group robots.
//
// The URL key is the entire credential. WeCom rejects bodies over 4096 bytes and limits the robot to 20 messages per minute, so bound both locally.
type weComChannel struct{}

func (weComChannel) Kind() string { return KindWeCom }

func (weComChannel) DefaultRatePerMin() int { return 20 }

// The webhook URL is the only secret field.
func (weComChannel) SecretKeys() []string { return []string{"webhook"} }

// The same field holds both the address and its credential.
func (weComChannel) DestinationKeys() []string { return []string{"webhook"} }

func (weComChannel) Validate(cfg map[string]any) error {
	hook := cfgString(cfg, "webhook")
	if hook == "" {
		return errors.New("缺少 Webhook 地址")
	}
	if err := validateHTTPURL(hook); err != nil {
		return fmt.Errorf("Webhook 地址无效: %w", err)
	}
	return nil
}

func (c weComChannel) Send(ctx context.Context, cfg map[string]any, m Message) (int, error) {
	if err := c.Validate(cfg); err != nil {
		return 0, Permanent(err)
	}
	// Pack whole batch entries under the byte limit before sending so WeCom does not reject the entire batch.
	content, kept := markdownBody(m, weComMarkdownLimit)
	payload := map[string]any{
		"msgtype":  "markdown",
		"markdown": map[string]any{"content": content},
	}
	raw, err := doJSON(ctx, "POST", cfgString(cfg, "webhook"), nil, payload)
	if err != nil {
		return 0, err
	}
	var res struct {
		ErrCode int    `json:"errcode"`
		ErrMsg  string `json:"errmsg"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return 0, fmt.Errorf("解析企业微信响应失败: %w (%s)", err, snippet(raw))
	}
	if res.ErrCode != 0 {
		// API code 45009 signals rate limiting and is retryable. Reduce the configured send rate if it recurs.
		if res.ErrCode == 45009 {
			return 0, fmt.Errorf("企业微信限流 %d: %s", res.ErrCode, res.ErrMsg)
		}
		// API code 93000 means the webhook key is invalid and delivery should not retry.
		return 0, Permanent(fmt.Errorf("企业微信返回错误 %d: %s", res.ErrCode, res.ErrMsg))
	}
	return kept, nil
}
