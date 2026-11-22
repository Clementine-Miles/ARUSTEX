package notify

import (
	"context"
	"errors"
	"sort"
	"strconv"
	"strings"
)

// Channel is a stateless notification adapter. Concurrent configurations share
// one instance, and each call supplies credentials in cfg.
type Channel interface {
	// Kind returns the channel key used in the registry.
	Kind() string
	// Validate checks required fields and formats before saving. Its errors are
	// shown to the operator, so identify the field that needs attention.
	Validate(cfg map[string]any) error
	// Send makes one delivery attempt and returns the number of items delivered.
	//
	// Channel size limits can truncate a digest. Mark only the first kept items
	// as delivered and leave the rest for another batch.
	//
	// On error, ignore kept. A *PermanentError tells the caller to skip retries.
	Send(ctx context.Context, cfg map[string]any, m Message) (int, error)
	// DefaultRatePerMin returns the default delivery limit for new channel instances.
	// Zero means no limit is known.
	DefaultRatePerMin() int
	// SecretKeys identifies credentials so the API can mask them and preserve saved
	// values on masked updates. Each adapter defines what its configuration treats
	// as a credential.
	SecretKeys() []string
	// DestinationKeys identifies configuration fields that control the receiver.
	//
	// Changing a receiver while retaining stored credentials can disclose the
	// credentials to the new receiver. See PrepareConfigUpdate.
	DestinationKeys() []string
}

// The explicit registry lists every supported adapter without init side effects.
var registry = map[string]Channel{
	KindDingTalk: dingTalkChannel{},
	KindFeishu:   feishuChannel{},
	KindWeCom:    weComChannel{},
	KindWebhook:  webhookChannel{},
	KindTelegram: telegramChannel{},
	KindEmail:    emailChannel{},
}

// Get looks up a channel adapter by kind.
func Get(kind string) (Channel, bool) {
	c, ok := registry[kind]
	return c, ok
}

// ValidKind reports whether kind is supported.
func ValidKind(kind string) bool {
	_, ok := registry[kind]
	return ok
}

// Kinds returns supported kinds in alphabetical order for the UI.
func Kinds() []string {
	out := make([]string, 0, len(registry))
	for k := range registry {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// PermanentError marks failures that retries cannot resolve, such as invalid
// credentials, rejected destinations, and invalid bodies.
type PermanentError struct{ Err error }

func (e *PermanentError) Error() string { return e.Err.Error() }
func (e *PermanentError) Unwrap() error { return e.Err }

// Permanent wraps err as a permanent failure and returns nil for nil input.
func Permanent(err error) error {
	if err == nil {
		return nil
	}
	return &PermanentError{Err: err}
}

// IsPermanent reports whether err wraps a permanent failure.
func IsPermanent(err error) bool {
	var pe *PermanentError
	return errors.As(err, &pe)
}

// Configuration readers.
//
// The database stores channel config as JSONB. The readers handle decoded
// float64 and []any values and tolerate strings entered through the UI.

// cfgString reads a string and trims surrounding whitespace.
func cfgString(cfg map[string]any, key string) string {
	v, ok := cfg[key]
	if !ok {
		return ""
	}
	s, ok := v.(string)
	if !ok {
		return ""
	}
	return strings.TrimSpace(s)
}

// cfgInt reads an integer from a JSON number or string.
func cfgInt(cfg map[string]any, key string) int {
	switch v := cfg[key].(type) {
	case float64:
		return int(v)
	case int:
		return v
	case string:
		n, err := strconv.Atoi(strings.TrimSpace(v))
		if err != nil {
			return 0
		}
		return n
	default:
		return 0
	}
}

// cfgBool reads a boolean or the strings "true" and "1".
func cfgBool(cfg map[string]any, key string) bool {
	switch v := cfg[key].(type) {
	case bool:
		return v
	case string:
		s := strings.ToLower(strings.TrimSpace(v))
		return s == "true" || s == "1" || s == "yes"
	default:
		return false
	}
}

// cfgStrings reads a string list, trimming whitespace and dropping empty values.
func cfgStrings(cfg map[string]any, key string) []string {
	raw, ok := cfg[key].([]any)
	if !ok {
		// A string also represents a single value in a form submission.
		if s := cfgString(cfg, key); s != "" {
			return []string{s}
		}
		return nil
	}
	out := make([]string, 0, len(raw))
	for _, v := range raw {
		s, ok := v.(string)
		if !ok {
			continue
		}
		if s = strings.TrimSpace(s); s != "" {
			out = append(out, s)
		}
	}
	return out
}

// cfgMap reads string key/value pairs, trimming both and dropping empty keys.
func cfgMap(cfg map[string]any, key string) map[string]string {
	raw, ok := cfg[key].(map[string]any)
	if !ok {
		return nil
	}
	out := make(map[string]string, len(raw))
	for k, v := range raw {
		k = strings.TrimSpace(k)
		if k == "" {
			continue
		}
		s, ok := v.(string)
		if !ok {
			continue
		}
		out[k] = s
	}
	return out
}
