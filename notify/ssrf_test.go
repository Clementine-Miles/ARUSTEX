package notify

import (
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync/atomic"
	"testing"
)

func TestTelegramDialGuardChecksResolvedIP(t *testing.T) {
	var hits atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hits.Add(1)
		_, _ = w.Write([]byte("{\"ok\":true}"))
	}))
	t.Cleanup(srv.Close)
	u, err := url.Parse(srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	u.Host = net.JoinHostPort("localhost", u.Port())
	cfg := testConfig(u.String())

	t.Setenv(AllowLocalTargetsEnv, "")
	_, err = Send(t.Context(), cfg, testMessage())
	if err == nil || hits.Load() != 0 || !strings.Contains(err.Error(), AllowLocalTargetsEnv) {
		t.Fatalf("unexpected default dial result: hits=%d err=%v", hits.Load(), err)
	}

	t.Setenv(AllowLocalTargetsEnv, "1")
	kept, err := Send(t.Context(), cfg, testMessage())
	if err != nil || kept != 1 || hits.Load() != 1 {
		t.Fatalf("explicit local opt-in failed: hits=%d kept=%d err=%v", hits.Load(), kept, err)
	}
}

func TestIsBlockedDialIP(t *testing.T) {
	blocked := []string{
		"127.0.0.1", "127.1.2.3", "::1",
		"169.254.0.1", "fe80::1",
		"0.0.0.0", "::",
		"224.0.0.1", "ff02::1",
		"::ffff:127.0.0.1",
		"",
	}
	for _, value := range blocked {
		if !isBlockedDialIP(net.ParseIP(value)) {
			t.Errorf("reserved address %q passed the dial guard", value)
		}
	}
	// Operator-managed Bot API relays can run on private networks.
	for _, value := range []string{"10.0.0.5", "172.16.3.4", "192.168.1.10", "192.0.2.1", "2001:db8::1"} {
		if isBlockedDialIP(net.ParseIP(value)) {
			t.Errorf("allowed address %s failed the dial guard", value)
		}
	}
}

func TestTelegramBaseURLValidation(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "")
	for _, raw := range []string{
		"http://127.0.0.1:8080/relay",
		"http://[::1]:8080/relay",
		"file:///relay",
		"https://relay.example.test/#fragment",
		"https://relay.example.test/?key=" + fixtureToken,
		"https://user:" + url.QueryEscape(fixtureToken) + "@relay.example.test",
		"https://relay.example.test/%zz?key=" + fixtureToken,
	} {
		err := ValidateConfig(testConfig(raw))
		assertNoToken(t, err)
	}
	for _, raw := range []string{"", "https://relay.example.test/prefix", "http://192.168.0.10/relay"} {
		if err := ValidateConfig(testConfig(raw)); err != nil {
			t.Errorf("rejected valid relay %q: %v", raw, err)
		}
	}
}
