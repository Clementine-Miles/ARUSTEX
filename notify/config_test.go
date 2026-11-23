package notify

import (
	"maps"
	"slices"
	"testing"
)

func TestTelegramConfigMasksOnlySupportedValues(t *testing.T) {
	stored := testConfig("https://relay.example.test")
	stored["unrecognized_secret"] = "synthetic-legacy-value"
	out := MaskConfig(stored)
	if out[botTokenKey] != MaskedPrefix+":…-token" || out[chatIDKey] != stored[chatIDKey] || out[baseURLKey] != stored[baseURLKey] {
		t.Fatalf("masked config = %v", out)
	}
	if _, ok := out["unrecognized_secret"]; ok {
		t.Fatal("unknown fields must not be returned")
	}
	if stored[botTokenKey] != fixtureToken {
		t.Fatal("masking modified stored values")
	}
	if !slices.Equal(SecretKeys(), []string{botTokenKey}) {
		t.Fatal("Telegram token is the only credential field")
	}
	legacy := testConfig("https://user:synthetic-relay-credential@relay.example.test")
	if _, ok := MaskConfig(legacy)[baseURLKey]; ok {
		t.Fatal("legacy relay URL with user information must not be returned")
	}
}

func TestTelegramConfigChangesRequireExplicitToken(t *testing.T) {
	stored := testConfig("https://first.example.test")
	for _, incoming := range []map[string]any{
		{baseURLKey: "https://second.example.test"},
		{baseURLKey: "", botTokenKey: MaskedPrefix},
		{baseURLKey: "", botTokenKey: ""},
	} {
		if _, err := PrepareConfigUpdate(stored, incoming); err == nil {
			t.Fatalf("relay change did not require an explicit token: %v", incoming)
		}
	}
	updated, err := PrepareConfigUpdate(stored, map[string]any{baseURLKey: "https://second.example.test", botTokenKey: "replacement-token"})
	if err != nil || updated[botTokenKey] != "replacement-token" || updated[baseURLKey] != "https://second.example.test" {
		t.Fatalf("update with explicitly supplied token = %v, %v", updated, err)
	}
}

func TestTelegramConfigRetainsMaskedTokenAcrossPartialUpdates(t *testing.T) {
	stored := map[string]any{botTokenKey: fixtureToken, chatIDKey: "1"}
	for range 3 {
		updated, err := PrepareConfigUpdate(stored, map[string]any{baseURLKey: " ", botTokenKey: MaskedPrefix})
		if err != nil || !maps.Equal(updated, stored) {
			t.Fatalf("round trip updated an omitted configuration value: %v, %v", updated, err)
		}
		stored = updated
	}
}

func TestTelegramConfigRejectsUnsupportedInput(t *testing.T) {
	for _, cfg := range []map[string]any{
		{"unrecognized": "value"},
		{botTokenKey: map[string]any{"value": MaskedPrefix}},
		{chatIDKey: 123},
	} {
		if _, err := PrepareConfigUpdate(nil, cfg); err == nil {
			t.Fatalf("accepted unsupported configuration: %v", cfg)
		}
	}
	for _, cfg := range []map[string]any{
		{chatIDKey: "1"},
		{botTokenKey: MaskedPrefix, chatIDKey: "1"},
		{botTokenKey: fixtureToken},
	} {
		if err := ValidateConfig(cfg); err == nil {
			t.Fatalf("accepted incomplete configuration: %v", cfg)
		}
	}
}
