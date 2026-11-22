package notify

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"strconv"
	"time"
)

// dingTalkChannel delivers messages through DingTalk custom bots.
//
// The client applies the 20 messages/minute limit because excess deliveries can
// be dropped without an HTTP error. The adapter supports unsigned bots and
// signature-based protection. Check errcode even when the HTTP status is 200.
type dingTalkChannel struct{}

func (dingTalkChannel) Kind() string { return KindDingTalk }

func (dingTalkChannel) DefaultRatePerMin() int { return 20 }

// Mask the webhook URL because it contains the access token.
func (dingTalkChannel) SecretKeys() []string { return []string{"webhook", "secret"} }

// The webhook URL identifies the destination. Changing it requires an explicit
// decision about the signing secret.
func (dingTalkChannel) DestinationKeys() []string { return []string{"webhook"} }

func (dingTalkChannel) Validate(cfg map[string]any) error {
	hook := cfgString(cfg, "webhook")
	if hook == "" {
		return errors.New("缺少 Webhook 地址")
	}
	if err := validateHTTPURL(hook); err != nil {
		return fmt.Errorf("Webhook 地址无效: %w", err)
	}
	return nil
}

// Send uses an ActionCard for a single item with a detail link and Markdown otherwise.
func (c dingTalkChannel) Send(ctx context.Context, cfg map[string]any, m Message) (int, error) {
	hook := cfgString(cfg, "webhook")
	if err := c.Validate(cfg); err != nil {
		return 0, Permanent(err)
	}
	endpoint, err := dingTalkSignedURL(hook, cfgString(cfg, "secret"), time.Now())
	if err != nil {
		return 0, Permanent(err)
	}

	title := markdownTitle(m)
	// Bound Markdown bodies to protect against unusually large evidence fields.
	text, kept := markdownBody(m, 20000)

	var payload any
	if !m.Batch && len(m.Items) == 1 && m.Items[0].DetailURL != "" {
		payload = map[string]any{
			"msgtype": "actionCard",
			"actionCard": map[string]any{
				"title":          title,
				"text":           text,
				"btnOrientation": "0",
				"singleTitle":    "查看详情",
				"singleURL":      m.Items[0].DetailURL,
			},
		}
	} else {
		payload = map[string]any{
			"msgtype":  "markdown",
			"markdown": map[string]any{"title": title, "text": text},
		}
	}

	raw, err := doJSON(ctx, "POST", endpoint, nil, payload)
	if err != nil {
		return 0, err
	}
	// Check for a DingTalk error in the HTTP 200 response body.
	var res struct {
		ErrCode int    `json:"errcode"`
		ErrMsg  string `json:"errmsg"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return 0, fmt.Errorf("解析钉钉响应失败: %w (%s)", err, snippet(raw))
	}
	if res.ErrCode != 0 {
		// Signature and keyword failures are permanent configuration errors.
		return 0, Permanent(fmt.Errorf("钉钉返回错误 %d: %s", res.ErrCode, res.ErrMsg))
	}
	return kept, nil
}

// dingTalkSignedURL adds the DingTalk timestamp and sign query parameters.
//
// Sign timestamp + "\n" + secret with HMAC-SHA256 keyed by secret. The timestamp
// is in milliseconds. Base64-encode the signature before URL encoding it.
// A blank secret leaves the webhook unchanged.
func dingTalkSignedURL(hook, secret string, now time.Time) (string, error) {
	if secret == "" {
		return hook, nil
	}
	ts := strconv.FormatInt(now.UnixMilli(), 10)
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(ts + "\n" + secret))
	sign := base64.StdEncoding.EncodeToString(mac.Sum(nil))

	u, err := url.Parse(hook)
	if err != nil {
		// Hide the parse error because it contains the webhook and its token.
		return "", fmt.Errorf("解析 Webhook 地址失败: %s", redactRequestTarget(hook))
	}
	q := u.Query()
	q.Set("timestamp", ts)
	q.Set("sign", sign)
	u.RawQuery = q.Encode()
	return u.String(), nil
}

// validateHTTPURL validates the scheme and rejects blocked IP literals.
//
// Validation has two boundaries.
//
// Return credential-free errors. A URL parse error can contain the raw webhook,
// which would otherwise reach API responses, delivery history, or logs.
//
// Reject blocked IP literals at configuration time. The dial guard checks
// directly resolved addresses, including changes caused by DNS rebinding.
//
// Limit delivery URLs to HTTP(S).
func validateHTTPURL(raw string) error {
	u, err := url.Parse(raw)
	if err != nil {
		return fmt.Errorf("地址无法解析（%s）", redactRequestTarget(raw))
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return fmt.Errorf("只支持 http/https，收到 %q", u.Scheme)
	}
	if u.Host == "" {
		return errors.New("缺少主机名")
	}
	if ip := net.ParseIP(u.Hostname()); ip != nil && isBlockedDialIP(ip) && !allowLocalTargets() {
		return fmt.Errorf("拒绝投递到本机/链路本地地址 %s（如确需投递到本机服务，设置 %s=1）", ip, AllowLocalTargetsEnv)
	}
	return nil
}
