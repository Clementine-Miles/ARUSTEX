package notify

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"syscall"
	"time"
)

// AllowLocalTargetsEnv opts in to delivery to otherwise blocked address classes.
//
// Block sensitive local targets by default. Delivery errors may include a
// response excerpt, so connecting to a local service can expose its data.
//
// ARTEX_NOTIFY_ALLOW_LOCAL=1 is an explicit opt-in for deployments that use
// local relays or receivers.
//
// Tests use AllowLocalTargetsEnv when delivering to local fixtures.
const AllowLocalTargetsEnv = "ARTEX_NOTIFY_ALLOW_LOCAL"

func allowLocalTargets() bool {
	v := strings.TrimSpace(os.Getenv(AllowLocalTargetsEnv))
	return v == "1" || strings.EqualFold(v, "true")
}

// isBlockedDialIP reports whether an address is blocked for direct delivery.
//
// Block loopback, link-local, unspecified, and multicast addresses. Private
// network addresses are permitted for operator-managed relays and webhooks.
func isBlockedDialIP(ip net.IP) bool {
	if ip == nil {
		return true
	}
	// Unmap IPv4-mapped IPv6 addresses before applying the IPv4 checks.
	if v4 := ip.To4(); v4 != nil {
		ip = v4
	}
	return ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() ||
		ip.IsInterfaceLocalMulticast() || ip.IsUnspecified() || ip.IsMulticast()
}

// blockInternalDial checks the destination address at connection time.
//
// Check each direct dial so a fresh DNS lookup or redirected request cannot
// bypass the configuration-time policy.
func blockInternalDial(_, address string, _ syscall.RawConn) error {
	if allowLocalTargets() {
		return nil
	}
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return err
	}
	ip := net.ParseIP(host)
	if ip == nil {
		return fmt.Errorf("无法解析目标地址 %q", host)
	}
	if isBlockedDialIP(ip) {
		return fmt.Errorf("拒绝投递到本机/链路本地地址 %s（如确需投递到本机服务，设置 %s=1）", ip, AllowLocalTargetsEnv)
	}
	return nil
}

// notifyTransport adds the dial guard while retaining the default pool, proxy,
// HTTP/2, and timeout settings.
var notifyTransport = func() *http.Transport {
	t, ok := http.DefaultTransport.(*http.Transport)
	if !ok {
		return &http.Transport{}
	}
	clone := t.Clone()
	clone.DialContext = (&net.Dialer{Timeout: 10 * time.Second, Control: blockInternalDial}).DialContext
	return clone
}()

// httpClient is shared by HTTP notification channels.
//
// Delivery uses its own client and a 15-second timeout, independent of the
// application's assessment proxy.
//
// Refuse redirects to a different host to protect credentials embedded in
// delivery URLs. Same-host redirects remain supported.
var httpClient = &http.Client{
	Timeout:   15 * time.Second,
	Transport: notifyTransport,
	CheckRedirect: func(req *http.Request, via []*http.Request) error {
		if len(via) >= 5 {
			return errors.New("重定向次数过多")
		}
		if len(via) > 0 && req.URL.Host != via[0].URL.Host {
			return fmt.Errorf("拒绝跨主机重定向（%s → %s）", via[0].URL.Host, req.URL.Host)
		}
		return nil
	},
}

// respBodyLimit caps response reads. Error reporting needs only a short excerpt.
const respBodyLimit = 8 << 10

// doJSON sends one request and returns its bounded response body.
//
// A nil payload sends no body. The generic webhook can supply custom headers.
//
// Network errors, 5xx, 408, and 429 are retryable. Other 4xx responses indicate
// permanent failures.
func doJSON(ctx context.Context, method, url string, headers map[string]string, payload any) ([]byte, error) {
	var body io.Reader
	if payload != nil {
		raw, err := json.Marshal(payload)
		if err != nil {
			// Encoding failures are local errors and will not improve with a retry.
			return nil, Permanent(fmt.Errorf("构造请求体失败: %w", err))
		}
		body = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, url, body)
	if err != nil {
		// Treat invalid URLs as permanent. Hide parse errors because they include the
		// original destination.
		return nil, Permanent(fmt.Errorf("请求地址非法: %s", redactRequestTarget(url)))
	}
	if payload != nil {
		req.Header.Set("Content-Type", "application/json; charset=utf-8")
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	resp, err := httpClient.Do(req)
	if err != nil {
		// Retry connection failures with backoff.
		//
		// Redact transport errors before exposing them to logs, delivery history or API
		// clients. The URL can itself contain a credential.
		return nil, fmt.Errorf("请求失败: %s", redactTransportError(err))
	}
	defer resp.Body.Close()
	raw, readErr := io.ReadAll(io.LimitReader(resp.Body, respBodyLimit))
	if readErr != nil {
		return nil, fmt.Errorf("读取响应失败: %w", readErr)
	}
	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		return raw, nil
	}
	// Retry rate limits and timeouts. The remaining 4xx responses are permanent.
	if resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode == http.StatusRequestTimeout {
		return nil, fmt.Errorf("对方限流或超时 (HTTP %d): %s", resp.StatusCode, snippet(raw))
	}
	if resp.StatusCode >= 500 {
		return nil, fmt.Errorf("对方服务异常 (HTTP %d): %s", resp.StatusCode, snippet(raw))
	}
	return nil, Permanent(fmt.Errorf("对方拒绝请求 (HTTP %d): %s", resp.StatusCode, snippet(raw)))
}

// snippet returns a short, single-line error excerpt for delivery history.
func snippet(raw []byte) string {
	return OneLine(string(raw), 200)
}

// redactRequestTarget retains only the scheme and host for diagnostics.
//
// Credentials may appear in any other part of a webhook URL.
//
// DingTalk and WeCom carry credentials in query parameters. Feishu and Telegram
// carry credentials in URL paths.
//
// Retaining the host is enough for network diagnostics without exposing the bot.
//
// Use a fixed placeholder for an unparseable address.
func redactRequestTarget(raw string) string {
	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return "(地址不可解析)"
	}
	return u.Scheme + "://" + u.Host + "/…"
}

// redactTransportError removes the request destination from a transport error.
//
// Unwrap *url.Error to avoid printing its raw URL.
func redactTransportError(err error) string {
	var uerr *url.Error
	if errors.As(err, &uerr) {
		host := ""
		if u, parseErr := url.Parse(uerr.URL); parseErr == nil {
			host = u.Host
		}
		if uerr.Err != nil {
			return fmt.Sprintf("%s %s: %s", uerr.Op, host, uerr.Err)
		}
		return fmt.Sprintf("%s %s: 未知错误", uerr.Op, host)
	}
	// Redact URL text in unstructured errors, including redirect errors.
	return redactURLsInText(err.Error())
}

// redactURLsInText redacts HTTP(S) URLs in a diagnostic message.
//
// This is a fallback for errors without structured URL fields. Quotes and
// whitespace delimit URL tokens.
func redactURLsInText(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); {
		rest := s[i:]
		if strings.HasPrefix(rest, "http://") || strings.HasPrefix(rest, "https://") {
			end := len(rest)
			if j := strings.IndexAny(rest, " \t\n\"'"); j >= 0 {
				end = j
			}
			b.WriteString(redactRequestTarget(rest[:end]))
			i += end
			continue
		}
		b.WriteByte(s[i])
		i++
	}
	return b.String()
}
