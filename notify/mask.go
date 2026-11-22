package notify

import (
	"encoding/json"
	"fmt"
	"strings"
)

// MaskedPrefix marks credentials in API responses. An incoming value with this prefix preserves the stored credential.
//
// MaskedValue can include a short hint so the user can identify a stored credential while editing.
const MaskedPrefix = "__masked__"

// MaskedValue returns a display placeholder for a stored credential.
//
// Short values use "__masked__" alone. Longer values include the last six characters as a hint.
//
// The suffix distinguishes credentials whose prefixes are shared, such as robot webhook URLs.
func MaskedValue(secret string) string {
	if len(secret) <= 6 {
		return MaskedPrefix
	}
	return MaskedPrefix + ":…" + secret[len(secret)-6:]
}

// IsMasked reports whether a value is a masked placeholder.
func IsMasked(v string) bool { return strings.HasPrefix(v, MaskedPrefix) }

// MaskConfig copies the configuration and replaces its credential fields with masked values.
//
// Return an empty map for unknown channel kinds to avoid disclosing raw credentials. Preserve ordinary fields for known channels.
func MaskConfig(kind string, cfg map[string]any) map[string]any {
	channel, ok := Get(kind)
	if !ok {
		return map[string]any{}
	}
	secrets := map[string]bool{}
	for _, k := range channel.SecretKeys() {
		secrets[k] = true
	}
	out := make(map[string]any, len(cfg))
	for k, v := range cfg {
		if !secrets[k] {
			out[k] = v
			continue
		}
		// Treat a nested structure such as headers as a single credential.
		if s, ok := v.(string); ok {
			out[k] = MaskedValue(s)
			continue
		}
		out[k] = MaskedPrefix
	}
	return out
}

// ErrDestinationChangedWithoutCredentials reports a destination change without explicit values for every credential field.
type ErrDestinationChangedWithoutCredentials struct {
	Changed []string // changed destination keys
	Missing []string // credential keys missing explicit values
}

func (e *ErrDestinationChangedWithoutCredentials) Error() string {
	return "目标地址（" + strings.Join(e.Changed, "、") + "）已变更，请同时重新填写凭据字段（" +
		strings.Join(e.Missing, "、") + "）：填入新值，或显式留空表示不再需要凭据。" +
		"原凭据只对旧地址有效，继续沿用等于把它交给新地址。"
}

// PrepareConfigUpdate merges configuration while guarding destination changes.
//
// MergeConfig otherwise preserves omitted credentials. A destination change must not forward those stored values to the new endpoint.
//
// This applies to webhook headers, Telegram URL tokens, and SMTP credentials.
//
// Destination checks apply before transport because this path requires no redirect.
//
// On a destination change, require a new value or explicit empty string for every credential key. Reject masked and omitted values.
//
// Silent removal would turn optional authentication into confusing delivery failures. Require the caller to make the choice explicitly.
func PrepareConfigUpdate(kind string, stored, incoming map[string]any) (map[string]any, error) {
	channel, ok := Get(kind)
	if !ok {
		return nil, fmt.Errorf("渠道类型 %q 未注册", kind)
	}
	secrets := channel.SecretKeys()
	destinations := channel.DestinationKeys()

	// Reject a masked placeholder inside a structured credential. MergeConfig recognizes a mask only as the entire value.
	//
	// Check before the unchanged-destination fast path so both branches enforce this invariant.
	if err := rejectMaskedInContainers(incoming, secrets); err != nil {
		return nil, err
	}

	// Find destination keys that changed. Masked values represent no change.
	var changed []string
	for _, key := range destinations {
		raw, present := incoming[key]
		if !present {
			continue
		}
		s, isStr := raw.(string)
		if isStr && IsMasked(s) {
			continue
		}
		if !sameConfigValue(raw, stored[key]) {
			changed = append(changed, key)
		}
	}
	if len(changed) == 0 {
		// Use the ordinary merge rules when the destination stays the same.
		return MergeConfig(stored, incoming), nil
	}

	// A destination change requires an explicit value for each credential.
	var missing []string
	for _, key := range secrets {
		raw, present := incoming[key]
		if !present {
			missing = append(missing, key)
			continue
		}
		if s, isStr := raw.(string); isStr && IsMasked(s) {
			missing = append(missing, key)
		}
	}
	if len(missing) > 0 {
		return nil, &ErrDestinationChangedWithoutCredentials{Changed: changed, Missing: missing}
	}
	return MergeConfig(stored, incoming), nil
}

// rejectMaskedInContainers rejects masked placeholders within structured values.
//
// A structured credential such as headers can be masked or submitted only as a whole.
func rejectMaskedInContainers(incoming map[string]any, secretKeys []string) error {
	for _, key := range secretKeys {
		raw, present := incoming[key]
		if !present {
			continue
		}
		if _, isStr := raw.(string); isStr {
			continue
		}
		encoded, err := json.Marshal(raw)
		if err != nil {
			continue
		}
		if strings.Contains(string(encoded), MaskedPrefix) {
			return fmt.Errorf("字段 %s 的内容里含掩码标记 %q：该字段只能整体留空表示沿用、或整体提交新值，不能在结构体内部夹带掩码占位",
				key, MaskedPrefix)
		}
	}
	return nil
}

// sameConfigValue compares JSON representations to tolerate differing Go numeric types for equal values.
//
// Normalize blank strings and absent keys because MergeConfig deletes a key when it receives an empty string.
//
// Repeated saves of an empty optional destination must not be mistaken for an origin change.
//
func sameConfigValue(a, b any) bool {
	if isBlankConfigValue(a) && isBlankConfigValue(b) {
		return true
	}
	ra, errA := json.Marshal(a)
	rb, errB := json.Marshal(b)
	if errA != nil || errB != nil {
		return false
	}
	return string(ra) == string(rb)
}

// isBlankConfigValue uses the same whitespace trimming as MergeConfig when identifying blank values.
func isBlankConfigValue(v any) bool {
	if v == nil {
		return true
	}
	s, ok := v.(string)
	return ok && strings.TrimSpace(s) == ""
}

// MergeConfig applies an incoming channel configuration patch to the stored configuration.
//
// Masked and omitted values preserve the old value. Empty strings delete keys. Other values replace the stored value.
//
// An explicit empty string lets the UI clear a field. Omitted fields retain partial-update semantics.
func MergeConfig(stored, incoming map[string]any) map[string]any {
	out := make(map[string]any, len(stored)+len(incoming))
	for k, v := range stored {
		out[k] = v
	}
	for k, v := range incoming {
		if s, ok := v.(string); ok {
			if IsMasked(s) {
				continue // masked values preserve the stored value
			}
			if strings.TrimSpace(s) == "" {
				delete(out, k)
				continue
			}
			out[k] = s
			continue
		}
		out[k] = v
	}
	return out
}
