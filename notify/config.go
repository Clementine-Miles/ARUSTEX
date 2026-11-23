package notify

import (
	"errors"
	"fmt"
	"slices"
	"strings"
)

const (
	botTokenKey = "bot_token"
	chatIDKey   = "chat_id"
	baseURLKey  = "base_url"

	// MaskedPrefix marks values returned to clients in place of credentials.
	MaskedPrefix = "__masked__"
)

var telegramConfigFields = [...]string{botTokenKey, chatIDKey, baseURLKey}

// SecretKeys returns the credential fields in a Telegram configuration.
func SecretKeys() []string { return []string{botTokenKey} }

// IsMasked reports whether a value requests preservation of a stored token.
func IsMasked(value string) bool { return strings.HasPrefix(value, MaskedPrefix) }

func configString(cfg map[string]any, key string) string {
	value, _ := cfg[key].(string)
	return strings.TrimSpace(value)
}

// MaskConfig returns the supported fields and replaces the token with a suffix hint.
func MaskConfig(cfg map[string]any) map[string]any {
	out := telegramConfig(cfg)
	if token := configString(out, botTokenKey); token != "" {
		masked := MaskedPrefix
		if len(token) > 6 {
			masked += ":…" + token[len(token)-6:]
		}
		out[botTokenKey] = masked
	}
	if base := configString(out, baseURLKey); base != "" {
		if _, err := parseTelegramBase(base); err != nil {
			delete(out, baseURLKey)
		}
	}
	return out
}

// PrepareConfigUpdate applies a partial update to a Telegram configuration.
// A different relay must receive an explicitly supplied bot token.
func PrepareConfigUpdate(stored, incoming map[string]any) (map[string]any, error) {
	out := telegramConfig(stored)
	for key, raw := range incoming {
		if !slices.Contains(telegramConfigFields[:], key) {
			return nil, errors.New("存在未知的 Telegram 配置字段")
		}
		value, ok := raw.(string)
		if !ok {
			return nil, fmt.Errorf("Telegram 配置字段 %q 必须是字符串", key)
		}
		value = strings.TrimSpace(value)
		if key == botTokenKey && IsMasked(value) {
			continue
		}
		if value == "" {
			delete(out, key)
		} else {
			out[key] = value
		}
	}
	if configString(stored, baseURLKey) != configString(out, baseURLKey) {
		token := configString(incoming, botTokenKey)
		if token == "" || IsMasked(token) {
			return nil, errors.New("目标地址（base_url）已变更，请同时重新填写凭据字段（bot_token）")
		}
	}
	return out, nil
}

func telegramConfig(cfg map[string]any) map[string]any {
	out := make(map[string]any, len(telegramConfigFields))
	for _, key := range telegramConfigFields {
		if value := configString(cfg, key); value != "" {
			out[key] = value
		}
	}
	return out
}
