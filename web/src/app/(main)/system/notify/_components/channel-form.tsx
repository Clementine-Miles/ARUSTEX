"use client";

import { CheckIcon } from "lucide-react";

import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { NotificationFilter, TelegramChannelConfig } from "@/lib/types";

export const SEVERITY_OPTIONS = [
  { value: "", label: "不限" },
  { value: "low", label: "低危及以上" },
  { value: "medium", label: "中危及以上" },
  { value: "high", label: "高危及以上" },
  { value: "critical", label: "仅严重" },
];

// Parse IDs separated by commas or whitespace.
export function parseIDs(text: string): number[] {
  return text
    .split(/[\s,，]+/)
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => Number(s))
    .filter((n) => Number.isFinite(n) && n > 0);
}
// Split keywords on commas or line breaks so finding types may contain spaces.
export function parseKeywords(text: string): string[] {
  return text
    .split(/[\n,，]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

export type ChannelForm = {
  name: string;
  mode: "realtime" | "digest";
  enabled: boolean;
  ratePerMin: string;
  config: Required<TelegramChannelConfig>;
  minSeverity: string;
  includeText: string;
  excludeText: string;
  taskIDsText: string;
  assetIDsText: string;
  onStatusChange: boolean;
};

export const emptyForm = (): ChannelForm => ({
  name: "",
  mode: "realtime",
  enabled: true,
  ratePerMin: "",
  config: { bot_token: "", chat_id: "", base_url: "" },
  minSeverity: "",
  includeText: "",
  excludeText: "",
  taskIDsText: "",
  assetIDsText: "",
  onStatusChange: false,
});

export function TelegramFields({
  config,
  onChange,
}: {
  config: Required<TelegramChannelConfig>;
  onChange: (patch: TelegramChannelConfig) => void;
}) {
  const masked = config.bot_token.startsWith("__masked__");
  const tokenTail = masked ? config.bot_token.split("…")[1] : undefined;

  return (
    <>
      <div className="grid gap-2">
        <Label htmlFor="n-bot-token">Bot Token</Label>
        <Input
          id="n-bot-token"
          type="password"
          placeholder="123456:ABC-DEF..."
          value={masked ? "" : config.bot_token}
          onChange={(e) => onChange({ bot_token: e.target.value })}
        />
        {masked && (
          <p className="flex items-center gap-1 text-muted-foreground text-xs">
            <CheckIcon className="size-3" />
            已保存{tokenTail ? `（尾号 ${tokenTail}）` : ""} · 填入新值可替换，更改 API 地址时须重新填写
          </p>
        )}
      </div>
      <div className="grid gap-2">
        <Label htmlFor="n-chat-id">Chat ID</Label>
        <Input
          id="n-chat-id"
          className="font-mono"
          placeholder="-1001234567890"
          value={config.chat_id}
          onChange={(e) => onChange({ chat_id: e.target.value })}
        />
      </div>
      <div className="grid gap-2">
        <Label htmlFor="n-api-url">API 地址</Label>
        <Input
          id="n-api-url"
          className="font-mono"
          placeholder="https://api.telegram.org"
          value={config.base_url}
          onChange={(e) => onChange({ base_url: e.target.value })}
        />
        <p className="text-muted-foreground text-xs">留空用官方地址；自建 Bot API 反代时填写。</p>
      </div>
    </>
  );
}

// Summarize notification filters without opening the channel card.
export function FilterSummary({ filter }: { filter: NotificationFilter }) {
  const parts: string[] = [];
  if (filter.min_severity) {
    parts.push(SEVERITY_OPTIONS.find((o) => o.value === filter.min_severity)?.label ?? filter.min_severity);
  }
  if (filter.vulnclass_include?.length) parts.push(`类型含 ${filter.vulnclass_include.length} 词`);
  if (filter.vulnclass_exclude?.length) parts.push(`排除 ${filter.vulnclass_exclude.length} 词`);
  if (filter.task_ids?.length) parts.push(`${filter.task_ids.length} 个任务`);
  if (filter.asset_ids?.length) parts.push(`${filter.asset_ids.length} 个资产`);
  if (filter.on_status_change) parts.push("含状态变更");
  if (parts.length === 0) {
    return <p className="text-muted-foreground text-sm">全部漏洞</p>;
  }
  return <p className="text-muted-foreground text-sm">{parts.join(" · ")}</p>;
}
