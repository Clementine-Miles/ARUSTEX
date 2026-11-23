package notify

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync/atomic"
	"testing"
)

func assertNoToken(t *testing.T, err error) {
	t.Helper()
	if err == nil {
		t.Fatal("expected a delivery error")
	}
	for _, token := range []string{fixtureToken, url.PathEscape(fixtureToken), url.QueryEscape(fixtureToken)} {
		if strings.Contains(err.Error(), token) {
			t.Fatal("error contains the synthetic bot token")
		}
	}
}

func TestTelegramErrorsDoNotEchoToken(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "1")
	t.Run("connection error", func(t *testing.T) {
		srv := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {}))
		base := srv.URL
		srv.Close()
		_, err := Send(t.Context(), testConfig(base), testMessage())
		assertNoToken(t, err)
	})
	t.Run("non-success HTTP status", func(t *testing.T) {
		srv := replyServer(t, http.StatusBadRequest, "invalid token "+url.QueryEscape(fixtureToken))
		_, err := Send(t.Context(), testConfig(srv.URL), testMessage())
		assertNoToken(t, err)
	})
	t.Run("Telegram response failure", func(t *testing.T) {
		body, err := json.Marshal(map[string]any{"ok": false, "error_code": 429, "description": "retry " + fixtureToken})
		if err != nil {
			t.Fatal(err)
		}
		srv := replyServer(t, http.StatusOK, string(body))
		_, err = Send(t.Context(), testConfig(srv.URL), testMessage())
		assertNoToken(t, err)
	})
	t.Run("malformed response", func(t *testing.T) {
		srv := replyServer(t, http.StatusOK, "invalid reply "+fixtureToken)
		_, err := Send(t.Context(), testConfig(srv.URL), testMessage())
		assertNoToken(t, err)
	})
}

func TestTelegramRedirectsStayOnOrigin(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "1")
	var otherOriginHits atomic.Int32
	other := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		otherOriginHits.Add(1)
		w.WriteHeader(http.StatusNoContent)
	}))
	t.Cleanup(other.Close)
	for _, tc := range []struct {
		name     string
		location string
		refused  bool
	}{
		{"same origin", "/relayed", false},
		{"different origin", other.URL + "/relayed", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.URL.Path == "/relayed" {
					_, _ = w.Write([]byte("{\"ok\":true}"))
					return
				}
				http.Redirect(w, r, tc.location, http.StatusTemporaryRedirect)
			}))
			t.Cleanup(srv.Close)
			kept, err := Send(t.Context(), testConfig(srv.URL), testMessage())
			if tc.refused {
				assertNoToken(t, err)
				if kept != 0 || otherOriginHits.Load() != 0 {
					t.Fatal("request left the configured origin")
				}
				return
			}
			if err != nil || kept != 1 {
				t.Fatalf("same-origin redirect returned %d, %v", kept, err)
			}
		})
	}
}
