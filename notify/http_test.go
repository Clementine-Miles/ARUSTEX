package notify

import (
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
)

// These tests cover Telegram transport error classification.

func replyServer(t *testing.T, status int, body string) *httptest.Server {
	t.Helper()
	t.Setenv(AllowLocalTargetsEnv, "1")
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = w.Write([]byte(body))
	}))
	t.Cleanup(srv.Close)
	return srv
}

func TestTelegramHTTPClassifiesHTTPStatus(t *testing.T) {
	cases := []struct {
		name      string
		status    int
		permanent bool
	}{
		{"200 成功不算错误", 200, false},
		{"429 限流可重试", 429, false},
		{"408 请求超时可重试", 408, false},
		{"500 服务端错误可重试", 500, false},
		{"502 网关错误可重试", 502, false},
		{"503 服务不可用可重试", 503, false},
		{"400 参数错误永久失败", 400, true},
		{"401 鉴权失败永久失败", 401, true},
		{"403 禁止访问永久失败", 403, true},
		{"404 地址不存在永久失败", 404, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := replyServer(t, tc.status, `{"detail":"upstream says no"}`)
			_, err := postTelegramJSON(t.Context(), srv.URL, "", map[string]any{})
			if tc.status < 300 {
				if err != nil {
					t.Fatalf("2xx 不应报错: %v", err)
				}
				return
			}
			if err == nil {
				t.Fatal("非 2xx 应报错")
			}
			if got := IsPermanent(err); got != tc.permanent {
				t.Fatalf("HTTP %d 的 permanent 判定错误：期望 %v 得到 %v (%v)",
					tc.status, tc.permanent, got, err)
			}
			// Assert the numeric status so the test is independent of translated wording.
			if !strings.Contains(err.Error(), strconv.Itoa(tc.status)) {
				t.Errorf("错误信息应带上 HTTP 状态码 %d，得到 %v", tc.status, err)
			}
		})
	}
}

// TestTelegramHTTPIncludesResponseSnippet checks that peer errors remain diagnosable.
func TestTelegramHTTPIncludesResponseSnippet(t *testing.T) {
	srv := replyServer(t, 400, `{"error":"invalid response payload"}`)
	_, err := postTelegramJSON(t.Context(), srv.URL, "", map[string]any{})
	if err == nil {
		t.Fatal("应报错")
	}
	if !strings.Contains(err.Error(), "invalid response payload") {
		t.Errorf("错误信息应带回对端的说明，得到 %v", err)
	}
}

// TestTelegramHTTPSnippetIsSingleLineAndBounded guards the shape of delivery history errors.
func TestTelegramHTTPSnippetIsSingleLineAndBounded(t *testing.T) {
	// Return an overlong response containing line breaks and tabs.
	long := strings.Repeat("x", 5000)
	srv := replyServer(t, 500, "line1\nline2\r\n\tline3 "+long)
	_, err := postTelegramJSON(t.Context(), srv.URL, "", map[string]any{})
	if err == nil {
		t.Fatal("应报错")
	}
	msg := err.Error()
	if strings.ContainsAny(msg, "\r\n\t") {
		t.Errorf("错误信息应压成单行，得到 %q", msg)
	}
	// Allow a 200-character snippet plus the fixed error prefix.
	if len(msg) > 400 {
		t.Errorf("错误信息过长（%d 字节），应被 snippet 截断: %q", len(msg), msg)
	}
}

// TestTelegramHTTPRejectsOversizedResponse verifies that delivery reads are bounded.
func TestTelegramHTTPRejectsOversizedResponse(t *testing.T) {
	huge := strings.Repeat("A", 1<<20) // 1 MiB
	srv := replyServer(t, 400, huge)
	_, err := postTelegramJSON(t.Context(), srv.URL, "", map[string]any{})
	if err == nil {
		t.Fatal("应报错")
	}
	if len(err.Error()) > 400 {
		t.Errorf("超大响应应被限长读取并截断，错误信息长度 %d", len(err.Error()))
	}
}
