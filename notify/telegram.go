package notify

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/url"
	"strings"
)

// DefaultRatePerMin is the delivery limit for new Telegram destinations.
const DefaultRatePerMin = 20

// ValidateConfig checks the fields needed to send Telegram notifications.
func ValidateConfig(cfg map[string]any) error {
	_, err := telegramEndpoint(cfg)
	return err
}

func telegramEndpoint(cfg map[string]any) (string, error) {
	token := configString(cfg, botTokenKey)
	if token == "" || IsMasked(token) {
		return "", errors.New("缺少 Bot Token")
	}
	if configString(cfg, chatIDKey) == "" {
		return "", errors.New("缺少 Chat ID")
	}
	base := configString(cfg, baseURLKey)
	if base == "" {
		base = "https://api.telegram.org"
	}
	u, err := parseTelegramBase(base)
	if err != nil {
		return "", fmt.Errorf("API 地址无效: %w", err)
	}
	if ip := net.ParseIP(u.Hostname()); !allowLocalTargets() && ip != nil && isBlockedDialIP(ip) {
		return "", localTargetError(ip)
	}
	u.RawPath = strings.TrimSuffix(u.EscapedPath(), "/") + "/bot" + url.PathEscape(token) + "/sendMessage"
	u.Path = strings.TrimSuffix(u.Path, "/") + "/bot" + token + "/sendMessage"
	return u.String(), nil
}

// Send delivers one Telegram message and returns the number of included items.
func Send(ctx context.Context, cfg map[string]any, m Message) (int, error) {
	endpoint, err := telegramEndpoint(cfg)
	if err != nil {
		return 0, &PermanentError{Err: err}
	}
	text, kept := telegramHTML(m)
	payload := map[string]any{
		"chat_id":                  configString(cfg, chatIDKey),
		"text":                     text,
		"parse_mode":               "HTML",
		"disable_web_page_preview": false,
	}
	token := configString(cfg, botTokenKey)
	raw, err := postTelegramJSON(ctx, endpoint, token, payload)
	if err != nil {
		return 0, err
	}
	var res struct {
		OK          bool   `json:"ok"`
		ErrorCode   int    `json:"error_code"`
		Description string `json:"description"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return 0, fmt.Errorf("解析 Telegram 响应失败: %s", responseSnippet(string(raw), token))
	}
	if res.OK {
		return kept, nil
	}
	// Retry rate limits with backoff. Other listed API errors signal configuration problems.
	if res.ErrorCode == 429 {
		return 0, fmt.Errorf("Telegram 限流: %s", responseSnippet(res.Description, token))
	}
	return 0, &PermanentError{Err: fmt.Errorf("Telegram 返回错误 %d: %s", res.ErrorCode, responseSnippet(res.Description, token))}
}

// parseTelegramBase accepts an HTTP(S) origin and an optional relay path prefix.
func parseTelegramBase(base string) (*url.URL, error) {
	u, err := url.Parse(base)
	if err != nil || u.Hostname() == "" {
		return nil, errors.New("地址格式无效")
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, errors.New("地址必须使用 HTTP 或 HTTPS")
	}
	if u.User != nil || u.ForceQuery || u.RawQuery != "" || u.Fragment != "" {
		return nil, errors.New("地址只能包含主机、端口和路径前缀")
	}
	return u, nil
}
