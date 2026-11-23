package notify

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"unicode/utf8"
)

const fixtureToken = "1234:synthetic-local-token"

func testConfig(base string) map[string]any {
	return map[string]any{botTokenKey: fixtureToken, chatIDKey: "-10042", baseURLKey: base}
}

func testMessage() Message {
	return Message{Items: []Item{{
		Name:      "发现 <标记>",
		Severity:  "high",
		Summary:   "第一行\n第二行 & 引号",
		DetailURL: "https://example.test/finding?task=1&item=2",
	}}}
}

func TestTelegramSendPOST(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "1")
	type request struct {
		method, path, contentType string
		body                      map[string]any
		err                       error
	}
	requests := make(chan request, 1)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		err := json.NewDecoder(r.Body).Decode(&body)
		requests <- request{r.Method, r.URL.Path, r.Header.Get("Content-Type"), body, err}
		_, _ = w.Write([]byte("{\"ok\":true}"))
	}))
	t.Cleanup(srv.Close)

	kept, err := Send(t.Context(), testConfig(srv.URL+"/relay/"), testMessage())
	if err != nil || kept != 1 {
		t.Fatalf("Send kept %d, err %v", kept, err)
	}
	r := <-requests
	if r.method != http.MethodPost || r.path != "/relay/bot"+fixtureToken+"/sendMessage" ||
		r.contentType != "application/json; charset=utf-8" || r.err != nil {
		t.Fatalf("unexpected Telegram request: %+v", r)
	}
	if r.body["chat_id"] != "-10042" || r.body["parse_mode"] != "HTML" {
		t.Fatalf("unexpected Telegram payload: %+v", r.body)
	}
	contents, _ := r.body["text"].(string)
	for _, expected := range []string{"发现 &lt;标记&gt;", "第一行 第二行 &amp; 引号", "task=1&amp;item=2"} {
		if !strings.Contains(contents, expected) {
			t.Errorf("payload is missing %q: %q", expected, contents)
		}
	}
}

func TestTelegramClassifiesAPIReply(t *testing.T) {
	t.Setenv(AllowLocalTargetsEnv, "1")
	cases := []struct {
		name      string
		reply     string
		permanent bool
	}{
		{"rate limit", "{\"ok\":false,\"error_code\":429}", false},
		{"invalid credentials", "{\"ok\":false,\"error_code\":401}", true},
		{"malformed response", "not JSON", false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := replyServer(t, http.StatusOK, tc.reply)
			kept, err := Send(t.Context(), testConfig(srv.URL), testMessage())
			if kept != 0 || err == nil || IsPermanent(err) != tc.permanent {
				t.Fatalf("Send kept %d, err %v; want retryable=%v", kept, err, !tc.permanent)
			}
		})
	}
}

func TestTelegramDigestCountsWholeEscapedItems(t *testing.T) {
	m := Message{Batch: true, WindowMinutes: 30, HomeURL: "https://example.test/findings"}
	for i := range 200 {
		m.Items = append(m.Items, Item{
			Name:     fmt.Sprintf("开始-%04d %s 结束-%04d", i, strings.Repeat("漏洞<", 25), i),
			Severity: "high",
		})
	}
	text, kept := telegramHTML(m)
	if kept < 1 || kept >= len(m.Items) {
		t.Fatalf("expected a partial digest, kept %d", kept)
	}
	if n := utf8.RuneCountInString(text); n > telegramTextLimit || !strings.Contains(text, "下一条继续") {
		t.Fatalf("digest exceeded the rune limit or omitted the remainder notice: %d runes", n)
	}
	for i := range kept {
		if !strings.Contains(text, fmt.Sprintf("结束-%04d", i)) {
			t.Fatalf("acknowledged row %d without its full text", i)
		}
	}
	if strings.Contains(text, fmt.Sprintf("开始-%04d", kept)) {
		t.Fatalf("message includes a partially packed item")
	}
}

func TestTelegramDigestKeepsAtLeastOneItem(t *testing.T) {
	m := Message{Batch: true, Items: []Item{
		{Name: strings.Repeat("漏洞", telegramTextLimit)},
		{Name: "next entry"},
	}}
	text, kept := telegramHTML(m)
	if kept != 1 || utf8.RuneCountInString(text) > telegramTextLimit || strings.Contains(text, "next entry") {
		t.Fatalf("oversized first entry kept %d with %d runes", kept, utf8.RuneCountInString(text))
	}
}
