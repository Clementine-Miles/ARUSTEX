package notify

import (
	"context"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"strings"
	"testing"
)

// These tests check notification dial policy and credential redaction in URL validation errors.
//
// TestMain permits loopback for local receivers. The denial tests clear the override to exercise the default policy.

func TestMain(m *testing.M) {
	// Enable local fixtures for ordinary tests.
	_ = os.Setenv(AllowLocalTargetsEnv, "1")
	os.Exit(m.Run())
}

// TestDialGuardRejectsLoopbackByDefault checks that the connection layer refuses loopback by default.
func TestDialGuardRejectsLoopbackByDefault(t *testing.T) {
	var hit bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		hit = true
		_, _ = io.WriteString(w, `{"errcode":0}`)
	}))
	defer srv.Close()

	t.Setenv(AllowLocalTargetsEnv, "") // clear the opt-in for default behavior
	_, err := (dingTalkChannel{}).Send(context.Background(),
		map[string]any{"webhook": srv.URL + "/robot/send"}, Message{Items: []Item{{Severity: "high"}}})
	if err == nil {
		t.Fatal("默认不应允许投递到环回地址")
	}
	if hit {
		t.Fatal("请求已经打到了本机服务——守卫没生效")
	}
	// Point users toward the explicit opt-in for legitimate local relays.
	if !strings.Contains(err.Error(), AllowLocalTargetsEnv) {
		t.Errorf("拒绝信息应说明如何显式放开: %v", err)
	}
}

// TestDialGuardAllowsLoopbackWhenOptedIn checks that an operator can use a local relay deliberately.
func TestDialGuardAllowsLoopbackWhenOptedIn(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(w, `{"errcode":0,"errmsg":"ok"}`)
	}))
	defer srv.Close()

	t.Setenv(AllowLocalTargetsEnv, "1")
	if _, err := (dingTalkChannel{}).Send(context.Background(),
		map[string]any{"webhook": srv.URL + "/robot/send"}, Message{Items: []Item{{Severity: "high"}}}); err != nil {
		t.Fatalf("显式放开后应可投递: %v", err)
	}
}

func TestIsBlockedDialIP(t *testing.T) {
	blocked := []string{
		"127.0.0.1", "127.1.2.3", "::1",
		"169.254.169.254", // cloud metadata address
		"169.254.1.1", "fe80::1",
		"0.0.0.0", "::",
		"224.0.0.1", "ff02::1",
		"::ffff:127.0.0.1", // unmap IPv4-mapped addresses before applying policy
		"",
	}
	for _, s := range blocked {
		if !isBlockedDialIP(net.ParseIP(s)) {
			t.Errorf("%s 应被拒绝", s)
		}
	}
	// RFC 1918 addresses are allowed for legitimate private messaging and SMTP relays. Keep this policy change deliberate.
	allowed := []string{"10.0.0.5", "172.16.3.4", "192.168.1.10", "8.8.8.8", "2606:4700::1111"}
	for _, s := range allowed {
		if isBlockedDialIP(net.ParseIP(s)) {
			t.Errorf("%s 应被放行（私网是常见的合法投递目标）", s)
		}
	}
}

// TestValidateHTTPURLRejectsLiteralPrivateTargets checks that blocked literal addresses are rejected when saving configuration.
func TestValidateHTTPURLRejectsLiteralPrivateTargets(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "")
	for _, raw := range []string{
		"http://127.0.0.1:8080/hook",
		"http://169.254.169.254/latest/meta-data/",
		"http://[::1]:8080/hook",
	} {
		if err := validateHTTPURL(raw); err == nil {
			t.Errorf("%s 应在配置阶段被拒绝", raw)
		}
	}
	// Allow public and RFC 1918 addresses.
	for _, raw := range []string{"https://oapi.dingtalk.com/robot/send", "http://10.0.0.9/hook"} {
		if err := validateHTTPURL(raw); err != nil {
			t.Errorf("%s 应通过校验: %v", raw, err)
		}
	}
}

// TestValidateHTTPURLErrorNeverLeaksCredentials covers parse failures specifically.
//
// url.Parse errors can contain the original URL. Unsupported schemes exercise a later validation branch, so use input that cannot parse.
func TestValidateHTTPURLErrorNeverLeaksCredentials(t *testing.T) {
	cases := []string{
		"http://127.0.0.1/%zz?access_token=" + leakProbeToken,         // invalid percent escape
		"https://a.example.com:port/x?access_token=" + leakProbeToken, // nonnumeric port
		"http://[::1?access_token=" + leakProbeToken,                  // unbalanced brackets
	}
	for _, raw := range cases {
		// Confirm the fixture reaches the intended parse error branch.
		if _, err := url.Parse(raw); err == nil {
			t.Errorf("%q 本应解析失败，否则这条用例没有覆盖到目标分支", raw)
			continue
		}
		err := validateHTTPURL(raw)
		if err == nil {
			t.Errorf("%q 应校验失败", raw)
			continue
		}
		assertNoSecret(t, err.Error(), leakProbeToken)
	}
	// The channel adapter must not echo the URL either.
	t.Setenv(AllowLocalTargetsEnv, "")
	err := (dingTalkChannel{}).Validate(map[string]any{"webhook": cases[0]})
	if err == nil {
		t.Fatal("非法地址应校验失败")
	}
	assertNoSecret(t, err.Error(), leakProbeToken)
}

// TestEmailDialGuardRejectsLoopbackByDefault checks the SMTP dial boundary.
//
// SMTP shares the dial guard because response and connection errors can expose information about blocked endpoints.
//
// Clear the TestMain loopback override before checking the default.
func TestEmailDialGuardRejectsLoopbackByDefault(t *testing.T) {
	f := newFakeSMTP(t)
	cfg := emailCfg(t, f, nil)

	t.Setenv(AllowLocalTargetsEnv, "") // clear the opt-in for default behavior
	_, err := (emailChannel{}).Send(context.Background(), cfg, singleMsg())
	if err == nil {
		t.Fatal("默认不应允许把邮件投递到环回地址")
	}
	// The dial hook rejects the connection before any SMTP exchange.
	if f.sawCommand("EHLO") || f.sawCommand("HELO") {
		t.Fatal("SMTP 会话已经建立——守卫没生效")
	}
	// The error should explain the opt-in for intentional local relays.
	if !strings.Contains(err.Error(), AllowLocalTargetsEnv) {
		t.Errorf("拒绝信息应说明如何显式放开: %v", err)
	}
}

// TestEmailDialGuardAllowsLoopbackWhenOptedIn checks that an explicitly permitted local relay still delivers.
func TestEmailDialGuardAllowsLoopbackWhenOptedIn(t *testing.T) {
	f := newFakeSMTP(t)
	cfg := emailCfg(t, f, nil)

	t.Setenv(AllowLocalTargetsEnv, "1")
	if _, err := (emailChannel{}).Send(context.Background(), cfg, singleMsg()); err != nil {
		t.Fatalf("显式放开后本机 SMTP 应可投递: %v", err)
	}
	if !f.sawCommand("EHLO") {
		t.Fatal("未看到 EHLO——会话没真正建立")
	}
}
