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

// AllowLocalTargetsEnv opts in to otherwise blocked targets for local relays.
const AllowLocalTargetsEnv = "ARTEX_NOTIFY_ALLOW_LOCAL"

func allowLocalTargets() bool {
	value := strings.TrimSpace(os.Getenv(AllowLocalTargetsEnv))
	return value == "1" || strings.EqualFold(value, "true")
}

// isBlockedDialIP allows private addresses for operator-managed relays while
// blocking loopback, link-local, unspecified, and multicast destinations.
func isBlockedDialIP(ip net.IP) bool {
	if ip == nil {
		return true
	}
	if v4 := ip.To4(); v4 != nil {
		ip = v4
	}
	return ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() ||
		ip.IsInterfaceLocalMulticast() || ip.IsUnspecified() || ip.IsMulticast()
}

func localTargetError(ip net.IP) error {
	return fmt.Errorf("拒绝投递到本机/链路本地地址 %s（如确需投递到本机服务，设置 %s=1）", ip, AllowLocalTargetsEnv)
}

// blockInternalDial checks the resolved IP on each direct dial.
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
		return localTargetError(ip)
	}
	return nil
}

var telegramClient = &http.Client{
	Timeout:   15 * time.Second,
	Transport: telegramTransport(),
	CheckRedirect: func(req *http.Request, via []*http.Request) error {
		if len(via) >= 5 {
			return errors.New("重定向次数过多")
		}
		if len(via) > 0 && (req.URL.Scheme != via[0].URL.Scheme || req.URL.Host != via[0].URL.Host || req.URL.User != nil) {
			return errors.New("拒绝跨来源重定向")
		}
		return nil
	},
}

func telegramTransport() *http.Transport {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	// Keep resolution and connection under the dial guard. A generic proxy
	// would resolve and connect to the relay on ARTEX's behalf.
	transport.Proxy = nil
	transport.DialContext = (&net.Dialer{Timeout: 10 * time.Second, Control: blockInternalDial}).DialContext
	return transport
}

// PermanentError marks a delivery failure that retries cannot resolve.
type PermanentError struct{ Err error }

func (e *PermanentError) Error() string { return e.Err.Error() }
func (e *PermanentError) Unwrap() error { return e.Err }

// IsPermanent reports whether err wraps a permanent delivery failure.
func IsPermanent(err error) bool {
	var permanent *PermanentError
	return errors.As(err, &permanent)
}

const responseLimit = 8 << 10

func postTelegramJSON(ctx context.Context, endpoint, token string, payload any) ([]byte, error) {
	body, err := json.Marshal(payload)
	if err != nil {
		return nil, &PermanentError{Err: fmt.Errorf("构造请求体失败: %w", err)}
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return nil, &PermanentError{Err: errors.New("Telegram 请求地址无效")}
	}
	req.Header.Set("Content-Type", "application/json; charset=utf-8")
	resp, err := telegramClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("请求失败: %s", redactTelegramError(transportErrorText(err), token))
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, responseLimit))
	if err != nil {
		return nil, fmt.Errorf("读取响应失败: %s", redactTelegramError(err.Error(), token))
	}
	switch {
	case resp.StatusCode >= 200 && resp.StatusCode < 300:
		return raw, nil
	case resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode == http.StatusRequestTimeout:
		return nil, fmt.Errorf("对方限流或超时 (HTTP %d): %s", resp.StatusCode, responseSnippet(string(raw), token))
	case resp.StatusCode >= 500:
		return nil, fmt.Errorf("对方服务异常 (HTTP %d): %s", resp.StatusCode, responseSnippet(string(raw), token))
	default:
		return nil, &PermanentError{Err: fmt.Errorf("对方拒绝请求 (HTTP %d): %s", resp.StatusCode, responseSnippet(string(raw), token))}
	}
}

func responseSnippet(body, token string) string {
	return oneLine(redactTelegramError(body, token), 200)
}

// transportErrorText unwraps the standard client error without printing the
// token-bearing request URL.
func transportErrorText(err error) string {
	var uerr *url.Error
	if errors.As(err, &uerr) {
		host := ""
		if u, parseErr := url.Parse(uerr.URL); parseErr == nil {
			host = u.Host
		}
		return fmt.Sprintf("%s %s: %v", uerr.Op, host, uerr.Err)
	}
	return err.Error()
}

// redactTelegramError removes the current token and URL path, query, and user
// info from diagnostics that can reach the API, logs, and delivery history.
func redactTelegramError(text, token string) string {
	if token != "" {
		text = strings.NewReplacer(
			token, "[redacted]",
			url.PathEscape(token), "[redacted]",
			url.QueryEscape(token), "[redacted]",
		).Replace(text)
	}
	var b strings.Builder
	for i := 0; i < len(text); {
		rest := text[i:]
		if !strings.HasPrefix(rest, "http://") && !strings.HasPrefix(rest, "https://") {
			b.WriteByte(text[i])
			i++
			continue
		}
		end := len(rest)
		if j := strings.IndexAny(rest, " \t\r\n\"'"); j >= 0 {
			end = j
		}
		if u, err := url.Parse(rest[:end]); err == nil && u.Host != "" {
			b.WriteString(u.Scheme + "://" + u.Host + "/…")
		} else {
			b.WriteString("(地址不可解析)")
		}
		i += end
	}
	return b.String()
}
