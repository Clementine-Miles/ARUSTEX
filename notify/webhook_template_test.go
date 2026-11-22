package notify

import (
	"context"
	"reflect"
	"sort"
	"strings"
	"testing"
)

// Tests of the webhook template capability boundary.
//
// Keep template data and helpers deliberately restricted. Adding a field type with methods or a new function can widen access.

// TestTemplateContextHasNoMethods checks the exposed type boundary.
//
// text/template can call exported methods. Expose only plain data with no methods.
//
// Before adding methods to the context, check what data and effects they expose to template authors.
func TestTemplateContextHasNoMethods(t *testing.T) {
	for _, v := range []any{webhookTemplateData{}, webhookItem{}} {
		typ := reflect.TypeOf(v)
		if n := typ.NumMethod(); n != 0 {
			var names []string
			for i := 0; i < n; i++ {
				names = append(names, typ.Method(i).Name)
			}
			t.Fatalf("%s 暴露了 %d 个方法（%s）：text/template 可以调用它们，"+
				"等于把这些方法的能力开放给了模板作者", typ.Name(), n, strings.Join(names, ", "))
		}
	}
}

// TestTemplateFuncsAreMinimal pins the functions exposed to templates.
//
// json and jsons serialize data. They expose no file, network, or execution capability.
func TestTemplateFuncsAreMinimal(t *testing.T) {
	var got []string
	for name := range webhookTemplateFuncs {
		got = append(got, name)
	}
	sort.Strings(got)
	want := []string{"json", "jsons"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("模板函数集合变了：得到 %v，期望 %v。新增函数前请确认它不会扩大能力面"+
			"（不能读写文件、不能发起网络请求、不能执行命令）", got, want)
	}
}

// TestTemplateCannotReachUnknownData checks that invalid references fail without disclosing internal values.
func TestTemplateCannotReachUnknownData(t *testing.T) {
	_, err := renderWebhookBody(`{"x": {{.Environment}}, "y": {{.Env}}}`, singleMsg())
	if err == nil {
		t.Fatal("访问不存在的字段应报错")
	}
	// Errors must not include finding titles or summaries from the context.
	for _, leak := range []string{"SQL注入", "参数 id"} {
		if strings.Contains(err.Error(), leak) {
			t.Errorf("模板错误泄露了消息内容 %q: %v", leak, err)
		}
	}
}

// TestTemplateRenderFailsPermanently verifies that template errors do not consume the retry budget.
func TestTemplateRenderFailsPermanently(t *testing.T) {
	cfg := map[string]any{
		"url":           "https://example.com/hook",
		"body_template": `{{.Items.`,
	}
	if err := (webhookChannel{}).Validate(cfg); err == nil {
		t.Fatal("模板语法错误应在保存时就被拦下")
	}
	// A render-time error still counts as permanent if validation was skipped.
	_, err := (webhookChannel{}).Send(context.Background(), cfg, singleMsg())
	if err == nil || !IsPermanent(err) {
		t.Fatalf("坏模板应判永久失败，得到 %v", err)
	}
}

// TestTemplateCanOnlyProduceJSON checks the output format constraint.
func TestTemplateCanOnlyProduceJSON(t *testing.T) {
	// Accept valid JSON templates.
	ok := map[string]any{"url": "https://example.com/hook", "body_template": `{"t":{{json .Title}}}`}
	if err := (webhookChannel{}).Validate(ok); err != nil {
		t.Fatalf("合法模板应通过校验: %v", err)
	}
	// Reject non-JSON results before sending.
	bad := map[string]any{"url": "http://127.0.0.1:1/hook", "body_template": `not json {{.Count}}`}
	_, err := (webhookChannel{}).Send(context.Background(), bad, singleMsg())
	if err == nil || !IsPermanent(err) {
		t.Fatalf("渲染出非 JSON 应判永久失败，得到 %v", err)
	}
	if !strings.Contains(err.Error(), "合法 JSON") {
		t.Errorf("错误信息应说明是 JSON 问题，得到 %v", err)
	}
}
