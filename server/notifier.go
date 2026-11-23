package server

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strings"
	"sync"
	"time"

	"github.com/Autumn-27/artex/db"
	"github.com/Autumn-27/artex/notify"
)

// Global notification settings keys.
const (
	// settingNotifyEnabled is a maintenance switch. It defaults to on, and delivery still requires
	// a configured channel.
	settingNotifyEnabled = "notify_enabled"
	// settingNotifyPublicBaseURL is the public base URL for finding-detail links. Leave it empty
	// to omit links.
	settingNotifyPublicBaseURL = "notify_public_base_url"
	// settingNotifyDigestMinutes is the notification digest interval in minutes.
	settingNotifyDigestMinutes = "notify_digest_interval_min"
)

const (
	// notifyTick is the polling interval and accounts for most of the delay between recording a
	// finding and dispatching it.
	notifyTick = 3 * time.Second
	// notifyLease must exceed the worst-case time to send claimed deliveries. Otherwise two
	// dispatchers can send the same rows.
	notifyLease = 3 * time.Minute
	// notifyFanOutPerTick bounds event dispatch per poll so enabling a channel cannot expand an
	// entire backlog at once.
	notifyFanOutPerTick = 200
	// notifyDefaultDigestMinutes is the default digest interval.
	notifyDefaultDigestMinutes = 30
	// notifyUnlimitedBurstPerTick caps deliveries per poll for channels without a rate limit to
	// avoid blocking a poll on a large backlog.
	notifyUnlimitedBurstPerTick = 50
	// Claim leases must outlast all sequential sends in a poll. Five sends at 30 seconds leave 30
	// seconds of margin on a three-minute lease. This prevents a second process from reclaiming
	// rows and incrementing their attempts while the first is still sending.
	// TestNotifyTickBudgetFitsWithinLease checks the relationship.
	notifyMaxSendsPerChannelPerTick = 5
	// notifySendTimeout bounds one delivery. Its product with the per-channel limit must fit
	// within notifyLease.
	notifySendTimeout = 30 * time.Second
)

// notifyBackoff indexes retry delays by attempted sends. The initial attempt plus retries must
// match db.MaxNotifyAttempts.
var notifyBackoff = []time.Duration{
	time.Second,
	5 * time.Second,
	30 * time.Second,
}

// Notifier runs independently from Scheduler. Notifications need a three-second polling interval,
// and a delivery failure must not block Agent triggers.
type Notifier struct {
	pg *db.DB

	// mu protects the per-channel rate-limit buckets.
	mu      sync.Mutex
	buckets map[int64]*notifyBucket
}

// notifyBucket replenishes each channel's send allowance at a stable rate to avoid bursts on
// fixed-window boundaries.
type notifyBucket struct {
	tokens   float64
	lastFill time.Time
}

func newNotifier(s *Server) *Notifier {
	return &Notifier{pg: s.m.pg, buckets: map[int64]*notifyBucket{}}
}

// Run polls until ctx ends. server.New starts it once.
func (n *Notifier) Run(ctx context.Context) {
	if n.pg == nil {
		return
	}
	t := time.NewTicker(notifyTick)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			n.step(ctx)
		}
	}
}

// step dispatches new events, then sends due deliveries. Log failures and let the next poll retry.
func (n *Notifier) step(ctx context.Context) {
	if !n.pg.GetBool(settingNotifyEnabled, true) {
		return
	}
	if _, _, err := n.pg.FanOutPendingEvents(ctx, notifyFanOutPerTick); err != nil {
		log.Printf("[notify] 分派事件失败: %v", err)
		return
	}
	channels, err := n.pg.ListNotificationChannels(ctx)
	if err != nil {
		log.Printf("[notify] 读取渠道失败: %v", err)
		return
	}
	baseURL := trimTrailingSlash(notifyPublicBaseURL(n.pg))
	for _, ch := range channels {
		if !ch.IsEnabled() {
			continue
		}
		// Limit outbound messages rather than finding count. A real-time message contains one
		// finding, while a digest counts as one send for the whole batch. Check allowance before
		// claiming rows because claiming increments their attempt count.
		now := time.Now()
		if ch.Mode == db.NotifyModeDigest {
			tokens, claimLimit := digestTickPlan()
			if n.takeTokens(ch.ID, ch.RatePerMin, tokens, now) <= 0 {
				continue
			}
			n.stepDigest(ctx, ch, claimLimit, baseURL)
			continue
		}
		allow := n.takeTokens(ch.ID, ch.RatePerMin, notifyMaxSendsPerChannelPerTick, now)
		if allow <= 0 {
			continue
		}
		n.stepRealtime(ctx, ch, allow, baseURL)
	}
}

// digestTickPlan returns one token for a digest send and an independent cap on the findings it can
// claim. Using the send allowance as the batch size makes low-rate digests degenerate into
// single-finding messages. TestDigestTickPlanDecouplesBatchSizeFromSendBudget covers this
// distinction.
func digestTickPlan() (tokens, claimLimit int) {
	return 1, db.MaxDigestBatchSize
}

// stepRealtime claims and sends one message per finding for the channel.
func (n *Notifier) stepRealtime(ctx context.Context, ch *db.NotificationChannel, allow int, baseURL string) {
	deliveries, err := n.pg.ClaimRealtimeDeliveries(ctx, ch.ID, allow, notifyLease)
	if err != nil {
		log.Printf("[notify] 领取实时投递失败 channel=%d: %v", ch.ID, err)
		return
	}
	if len(deliveries) == 0 {
		return
	}
	cfg := readNotifyConfig(ch.Config)
	for _, dl := range deliveries {
		msg, err := n.renderSingle(ctx, dl, baseURL)
		if err != nil {
			// Mark local rendering failures without retry because another attempt cannot fix the
			// snapshot.
			_ = n.pg.FailDeliveries(ctx, []int64{dl.ID}, err.Error())
			continue
		}
		n.send(ctx, cfg, msg, []*db.NotificationDelivery{dl})
	}
}

// stepDigest aggregates due deliveries for the channel into one message.
func (n *Notifier) stepDigest(ctx context.Context, ch *db.NotificationChannel, allow int, baseURL string) {
	window := time.Duration(notifyDigestIntervalMin(n.pg)) * time.Minute
	due, err := n.pg.DigestBatchDue(ctx, ch.ID, window)
	if err != nil {
		log.Printf("[notify] 判断汇总批次失败 channel=%d: %v", ch.ID, err)
		return
	}
	if !due {
		return
	}
	deliveries, err := n.pg.ClaimDigestBatch(ctx, ch.ID, allow, notifyLease)
	if err != nil {
		log.Printf("[notify] 领取汇总批次失败 channel=%d: %v", ch.ID, err)
		return
	}
	if len(deliveries) == 0 {
		return
	}
	msg, included, err := n.renderBatch(ctx, deliveries, baseURL, int(window.Minutes()))
	if err != nil {
		_ = n.pg.FailDeliveries(ctx, deliveryIDs(deliveries), err.Error())
		return
	}
	// Explicitly fail entries with invalid snapshots. Otherwise they stay in sending state and are
	// reclaimed after every lease expires.
	if skipped := excludeDeliveries(deliveries, included); len(skipped) > 0 {
		reason := "事件快照无法解析，本条漏洞无法渲染成消息"
		if fErr := n.pg.FailDeliveries(ctx, deliveryIDs(skipped), reason); fErr != nil {
			log.Printf("[notify] 标记坏快照投递失败 channel=%d ids=%v: %v", ch.ID, deliveryIDs(skipped), fErr)
		}
		log.Printf("[notify] 跳过 %d 条快照无法解析的投递 channel=%d", len(skipped), ch.ID)
	}
	// Pass only rendered entries to send. Their order matches msg.Items so partial acknowledgement
	// marks the correct delivery rows.
	n.send(ctx, readNotifyConfig(ch.Config), msg, included)
}

// send applies one result to the rendered batch. If a channel fits only the first K items, it
// acknowledges those entries and leaves the remainder queued for the next send.
func (n *Notifier) send(ctx context.Context, cfg map[string]any, msg notify.Message, deliveries []*db.NotificationDelivery) {
	// Bound an individual send so a slow channel cannot block all remaining channels.
	sendCtx, cancel := context.WithTimeout(ctx, notifySendTimeout)
	defer cancel()
	delivered, err := notify.Send(sendCtx, cfg, msg)
	channelID := deliveries[0].ChannelID
	if err == nil && delivered > 0 {
		if delivered > len(deliveries) {
			// Clamp impossible acknowledgement counts and log the rendering discrepancy.
			log.Printf("[notify] 渠道回报送达条数 %d 超过投递数 %d channel=%d，按全部送达处理",
				delivered, len(deliveries), channelID)
			delivered = len(deliveries)
		}
		sent, rest := deliveries[:delivered], deliveries[delivered:]
		if err := n.pg.MarkDeliveriesSent(ctx, deliveryIDs(sent)); err != nil {
			log.Printf("[notify] 标记已送达失败 channel=%d ids=%v: %v", channelID, deliveryIDs(sent), err)
		}
		if len(rest) > 0 {
			// Requeue items that did not fit in the message. DeferDeliveries restores the attempt
			// consumed at claim time because these entries were not sent.
			if err := n.pg.DeferDeliveries(ctx, deliveryIDs(rest),
				fmt.Sprintf("本条消息已达渠道长度上限，仅送达前 %d 条，其余留待下一批", delivered)); err != nil {
				log.Printf("[notify] 分段续发排队失败 channel=%d ids=%v: %v", channelID, deliveryIDs(rest), err)
			}
		}
		return
	}
	if err == nil {
		// Treat a send with no error and zero acknowledgements as a failure so it backs off.
		err = fmt.Errorf("渠道未报告送达条数（delivered=%d）", delivered)
	}

	// Evaluate retry exhaustion per row. A nearly exhausted item must not use up the retry budget
	// of a new item in the same batch.
	permanent := notify.IsPermanent(err)
	var failIDs, exhaustedIDs []int64
	byDelay := map[time.Duration][]int64{}
	for _, dl := range deliveries {
		switch {
		case permanent:
			failIDs = append(failIDs, dl.ID)
		case dl.Attempts >= db.MaxNotifyAttempts:
			exhaustedIDs = append(exhaustedIDs, dl.ID)
		default:
			delay := notifyBackoff[min(dl.Attempts, len(notifyBackoff)-1)]
			byDelay[delay] = append(byDelay[delay], dl.ID)
		}
	}

	if len(failIDs) > 0 {
		if fErr := n.pg.FailDeliveries(ctx, failIDs, err.Error()); fErr != nil {
			log.Printf("[notify] 标记失败状态出错 channel=%d ids=%v: %v", channelID, failIDs, fErr)
		}
	}
	if len(exhaustedIDs) > 0 {
		reason := fmt.Sprintf("重试 %d 次后仍失败: %s", db.MaxNotifyAttempts, err)
		if fErr := n.pg.FailDeliveries(ctx, exhaustedIDs, reason); fErr != nil {
			log.Printf("[notify] 标记失败状态出错 channel=%d ids=%v: %v", channelID, exhaustedIDs, fErr)
		}
	}
	// Group retries by the few supported delays to avoid one database round trip per row.
	for delay, group := range byDelay {
		if rErr := n.pg.RescheduleDeliveries(ctx, group, delay, err.Error()); rErr != nil {
			log.Printf("[notify] 重排投递失败 channel=%d ids=%v: %v", channelID, group, rErr)
		}
	}
	if len(failIDs)+len(exhaustedIDs) > 0 {
		log.Printf("[notify] 投递失败 channel=%d 永久失败=%d 重试耗尽=%d 待重试=%d: %s",
			channelID, len(failIDs), len(exhaustedIDs), len(byDelay), err)
	}
}

// excludeDeliveries returns entries from all missing from keep by pointer identity. Use it to
// identify rows that did not render and still need a terminal status.
func excludeDeliveries(all, keep []*db.NotificationDelivery) []*db.NotificationDelivery {
	inKeep := make(map[*db.NotificationDelivery]bool, len(keep))
	for _, dl := range keep {
		inKeep[dl] = true
	}
	var out []*db.NotificationDelivery
	for _, dl := range all {
		if !inKeep[dl] {
			out = append(out, dl)
		}
	}
	return out
}

// readNotifyConfig decodes a stored destination. Invalid data is treated as missing configuration.
func readNotifyConfig(raw json.RawMessage) map[string]any {
	var cfg map[string]any
	if err := json.Unmarshal(raw, &cfg); err != nil {
		return nil
	}
	return cfg
}

// renderSingle builds a message from one finding.
func (n *Notifier) renderSingle(ctx context.Context, dl *db.NotificationDelivery, baseURL string) (notify.Message, error) {
	snap, err := parseSnapshot(dl)
	if err != nil {
		return notify.Message{}, err
	}
	item, err := n.itemFor(ctx, snap, baseURL)
	if err != nil {
		return notify.Message{}, err
	}
	return notify.Message{Items: []notify.Item{item}, HomeURL: baseURL}, nil
}

// renderBatch skips invalid snapshots and keeps included[i] aligned with msg.Items[i]. The caller
// needs that alignment when a channel accepts only a prefix. stepDigest marks invalid rows as
// failed.
func (n *Notifier) renderBatch(ctx context.Context, deliveries []*db.NotificationDelivery, baseURL string, windowMinutes int) (notify.Message, []*db.NotificationDelivery, error) {
	items := make([]notify.Item, 0, len(deliveries))
	included := make([]*db.NotificationDelivery, 0, len(deliveries))
	for _, dl := range deliveries {
		snap, err := parseSnapshot(dl)
		if err != nil {
			// Leave invalid snapshots out of both results so the caller can mark them failed.
			log.Printf("[notify] 汇总批次中跳过无法解析的快照 delivery=%d: %v", dl.ID, err)
			continue
		}
		item, err := n.itemFor(ctx, snap, baseURL)
		if err != nil {
			return notify.Message{}, nil, err
		}
		items = append(items, item)
		included = append(included, dl)
	}
	if len(items) == 0 {
		return notify.Message{}, nil, fmt.Errorf("汇总批次 %d 条投递全部无法解析", len(deliveries))
	}
	return notify.Message{
		Items:         items,
		Batch:         true,
		WindowMinutes: windowMinutes,
		HomeURL:       baseURL,
	}, included, nil
}

// itemFor renders the event snapshot and resolves its asset display name and detail link.
func (n *Notifier) itemFor(ctx context.Context, snap notify.Snapshot, baseURL string) (notify.Item, error) {
	assets, err := n.pg.NotificationAssetNames(ctx, snap.AssetIDs)
	if err != nil {
		// Send the notification without an asset name if that lookup fails.
		log.Printf("[notify] 解析资产名失败 finding=%d: %v", snap.FindingID, err)
	}
	item := notify.Item{
		FindingID:  snap.FindingID,
		Name:       snap.Name,
		VulnClass:  snap.VulnClass,
		Severity:   snap.Severity,
		Summary:    snap.Summary,
		Assets:     assets,
		FromStatus: snap.FromStatus,
		ToStatus:   snap.ToStatus,
	}
	if baseURL != "" {
		// The finding detail page accepts the finding ID in the id query parameter.
		item.DetailURL = fmt.Sprintf("%s/function/findings/detail?id=%d", baseURL, snap.FindingID)
	}
	return item, nil
}

// takeTokens grants at most want messages from the channel bucket. A digest costs one send.
// Capacity is the configured per-minute rate, and unlimited channels get a finite allowance per
// poll. Keeping unused tokens lets later polls use the accumulated burst capacity.
func (n *Notifier) takeTokens(channelID int64, ratePerMin, want int, now time.Time) int {
	if want <= 0 {
		return 0
	}
	if ratePerMin <= 0 {
		return min(want, notifyUnlimitedBurstPerTick)
	}
	n.mu.Lock()
	defer n.mu.Unlock()
	b := n.buckets[channelID]
	if b == nil {
		b = &notifyBucket{tokens: float64(ratePerMin), lastFill: now}
		n.buckets[channelID] = b
	}
	// Refill by elapsed wall-clock time at ratePerMin/60 tokens per second.
	if elapsed := now.Sub(b.lastFill).Seconds(); elapsed > 0 {
		b.tokens = min(float64(ratePerMin), b.tokens+elapsed*float64(ratePerMin)/60)
		b.lastFill = now
	}
	// Add a small epsilon before truncation so floating-point rounding does not strand a full
	// token.
	take := min(int(b.tokens+1e-9), want)
	if take <= 0 {
		return 0
	}
	b.tokens -= float64(take)
	return take
}

// parseSnapshot decodes the delivery's associated event snapshot.
func parseSnapshot(dl *db.NotificationDelivery) (notify.Snapshot, error) {
	var snap notify.Snapshot
	if len(dl.Snapshot) == 0 {
		return snap, fmt.Errorf("投递 %d 的事件快照为空", dl.ID)
	}
	if err := json.Unmarshal(dl.Snapshot, &snap); err != nil {
		return snap, fmt.Errorf("解析投递 %d 的事件快照失败: %w", dl.ID, err)
	}
	if snap.Kind == "" {
		// Use the stored event type because older snapshots may disagree.
		snap.Kind = dl.EventKind
	}
	return snap, nil
}

func deliveryIDs(deliveries []*db.NotificationDelivery) []int64 {
	out := make([]int64, 0, len(deliveries))
	for _, dl := range deliveries {
		out = append(out, dl.ID)
	}
	return out
}

func trimTrailingSlash(s string) string {
	return strings.TrimRight(s, "/")
}
