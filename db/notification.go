package db

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"strings"
	"time"

	"github.com/Autumn-27/artex/notify"
)

// Telegram channel configuration and event storage. Delivery claims and state transitions live
// in notification_delivery.go.
//
// Preserve these event-production and filtering invariants.
//
// The finding transaction only inserts an event. Querying channels or filters there could make
// notification misconfiguration block storage. Malformed filters match by default in
// notify.Match so a configuration error does not silently drop delivery.

// ErrNotificationChannelNotFound indicates a missing channel.
var ErrNotificationChannelNotFound = errors.New("通知渠道不存在")

// Delivery states.
const (
	NotifyStatePending = "pending" // Pending send.
	NotifyStateSending = "sending" // Claimed by a dispatcher with an active lease.
	NotifyStateSent    = "sent"    // Delivered.
	NotifyStateFailed  = "failed"  // Permanent failure or exhausted retries. Manual retry remains possible.
	NotifyStateSkipped = "skipped" // Skipped because the channel is disabled.
)

// Delivery modes.
const (
	NotifyModeRealtime = "realtime"
	NotifyModeDigest   = "digest"
)

// ValidNotifyMode validates the modes in application code to keep schema extension simple.
func ValidNotifyMode(m string) bool {
	return m == NotifyModeRealtime || m == NotifyModeDigest
}

// NotificationChannel stores a Telegram destination with JSONB configuration and filters.
type NotificationChannel struct {
	ID     int64           `json:"id"`
	Name   string          `json:"name"`
	Mode   string          `json:"mode"`
	Config json.RawMessage `json:"config"`
	Filter json.RawMessage `json:"filter"`
	// A pointer distinguishes an omitted flag from explicitly disabling the channel.
	Enabled    *bool     `json:"enabled,omitempty"`
	RatePerMin int       `json:"rate_per_min"`
	CreatedAt  time.Time `json:"created_at"`
	UpdatedAt  time.Time `json:"updated_at"`
}

// IsEnabled treats an unloaded, nil flag as enabled.
func (c *NotificationChannel) IsEnabled() bool { return c.Enabled == nil || *c.Enabled }

const notificationChannelCols = `id, name, enabled, config, mode, filter, rate_per_min, created_at, updated_at`

func scanNotificationChannel(sc interface{ Scan(...any) error }) (*NotificationChannel, error) {
	var c NotificationChannel
	var enabled bool
	if err := sc.Scan(&c.ID, &c.Name, &enabled, &c.Config, &c.Mode, &c.Filter, &c.RatePerMin, &c.CreatedAt, &c.UpdatedAt); err != nil {
		return nil, err
	}
	c.Enabled = &enabled
	return &c, nil
}

// ListNotificationChannels lists Telegram destinations. Legacy rows remain stored but inactive.
func (d *DB) ListNotificationChannels(ctx context.Context) ([]*NotificationChannel, error) {
	rows, err := d.QueryContext(ctx, `SELECT `+notificationChannelCols+` FROM notification_channels
WHERE kind=$1 ORDER BY enabled DESC, id`, notify.KindTelegram)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := []*NotificationChannel{}
	for rows.Next() {
		c, err := scanNotificationChannel(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, c)
	}
	return out, rows.Err()
}

// NotificationChannelByID retrieves a Telegram destination.
func (d *DB) NotificationChannelByID(ctx context.Context, id int64) (*NotificationChannel, error) {
	row := d.QueryRowContext(ctx, `SELECT `+notificationChannelCols+` FROM notification_channels WHERE id=$1 AND kind=$2`, id, notify.KindTelegram)
	c, err := scanNotificationChannel(row)
	if err == sql.ErrNoRows {
		return nil, ErrNotificationChannelNotFound
	}
	return c, err
}

// SaveNotificationChannel stores a Telegram destination. Disabling a destination skips unsent
// deliveries so re-enabling it does not send an obsolete backlog.
func (d *DB) SaveNotificationChannel(ctx context.Context, c *NotificationChannel) (int64, error) {
	if c.Mode == "" {
		c.Mode = NotifyModeRealtime
	}
	// Preserve zero as a valid unlimited rate.
	//
	// A write-time default would collapse an explicit zero into a nonzero rate and silently throttle
	// an unlimited channel.
	//
	// The server fills the default only when the create request omits the field.
	if c.RatePerMin < 0 {
		return 0, errors.New("限流值不能为负")
	}
	if c.Config == nil {
		c.Config = json.RawMessage(`{}`)
	}
	if c.Filter == nil {
		c.Filter = json.RawMessage(`{}`)
	}
	enabled := c.IsEnabled()

	if c.ID == 0 {
		var id int64
		err := d.QueryRowContext(ctx, `INSERT INTO notification_channels(name,kind,enabled,config,mode,filter,rate_per_min)
VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id`,
			c.Name, notify.KindTelegram, enabled, string(c.Config), c.Mode, string(c.Filter), c.RatePerMin).Scan(&id)
		return id, err
	}
	tx, err := d.BeginTx(ctx, nil)
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	var wasEnabled bool
	err = tx.QueryRowContext(ctx, `SELECT enabled FROM notification_channels WHERE id=$1 AND kind=$2 FOR UPDATE`, c.ID, notify.KindTelegram).Scan(&wasEnabled)
	if err == sql.ErrNoRows {
		return 0, ErrNotificationChannelNotFound
	}
	if err != nil {
		return 0, err
	}
	if _, err := tx.ExecContext(ctx, `UPDATE notification_channels
SET name=$2, enabled=$3, config=$4, mode=$5, filter=$6, rate_per_min=$7 WHERE id=$1`,
		c.ID, c.Name, enabled, string(c.Config), c.Mode, string(c.Filter), c.RatePerMin); err != nil {
		return 0, err
	}
	if wasEnabled && !enabled {
		if _, err := tx.ExecContext(ctx, `UPDATE notification_deliveries SET state=$2, last_error=$3
WHERE channel_id=$1 AND state IN ($4,$5)`, c.ID, NotifyStateSkipped, "渠道已停用", NotifyStatePending, NotifyStateSending); err != nil {
			return 0, err
		}
	}
	return c.ID, tx.Commit()
}

// DeleteNotificationChannel removes a channel and cascades to its delivery history.
func (d *DB) DeleteNotificationChannel(ctx context.Context, id int64) error {
	res, err := d.ExecContext(ctx, `DELETE FROM notification_channels WHERE id=$1 AND kind=$2`, id, notify.KindTelegram)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotificationChannelNotFound
	}
	return nil
}

// RecordNotificationEventTx attempts to queue an event in the caller's transaction.
//
// The finding path makes one insert without reading filters or channels. A successful insert and
// the finding commit atomically.
//
// The savepoint preserves finding writes when optional notification storage fails.
//
// PostgreSQL aborts a transaction on statement failure. Roll back to a savepoint so the caller
// can still commit.
//
// If event storage fails, log and drop that notification while retaining the finding. The
// boolean reports whether enqueue succeeded without treating it as a finding error.
func RecordNotificationEventTx(ctx context.Context, tx *sql.Tx, kind string, findingID int64, snap notify.Snapshot) bool {
	raw, err := json.Marshal(snap)
	if err != nil {
		log.Printf("[notify] 序列化推送事件失败 finding=%d: %v", findingID, err)
		return false
	}
	if _, err := tx.ExecContext(ctx, `SAVEPOINT notify_event`); err != nil {
		log.Printf("[notify] 建立保存点失败 finding=%d: %v", findingID, err)
		return false
	}
	if _, err := tx.ExecContext(ctx, `INSERT INTO notification_events(kind,finding_id,snapshot) VALUES($1,$2,$3)`,
		kind, findingID, string(raw)); err != nil {
		log.Printf("[notify] 写入推送事件失败 finding=%d（漏洞记录不受影响）: %v", findingID, err)
		// Roll back to the savepoint to clear the failed transaction state.
		if _, rbErr := tx.ExecContext(ctx, `ROLLBACK TO SAVEPOINT notify_event`); rbErr != nil {
			log.Printf("[notify] 回滚到保存点失败 finding=%d: %v", findingID, rbErr)
		}
		return false
	}
	// Release the savepoint after the insert.
	_, _ = tx.ExecContext(ctx, `RELEASE SAVEPOINT notify_event`)
	return true
}

// AddNotificationEvent queues an event in a standalone transaction, for callers without an
// existing transaction.
func (d *DB) AddNotificationEvent(ctx context.Context, kind string, findingID int64, snap notify.Snapshot) (int64, error) {
	raw, err := json.Marshal(snap)
	if err != nil {
		return 0, fmt.Errorf("序列化通知事件快照失败: %w", err)
	}
	var id int64
	err = d.QueryRowContext(ctx, `INSERT INTO notification_events(kind,finding_id,snapshot) VALUES($1,$2,$3) RETURNING id`,
		kind, findingID, string(raw)).Scan(&id)
	return id, err
}

// FanOutPendingEvents expands pending findings into deliveries for enabled channels. Return
// processed event and inserted delivery counts.
//
// Use one transaction and FOR UPDATE SKIP LOCKED so concurrent dispatchers claim distinct
// events.
//
// Evaluate the configured JSONB filters in Go. A small set of channels can be loaded and matched
// in memory without embedding filter logic in SQL.
//
// Mark even unmatched events processed so later polls do not rescan them.
func (d *DB) FanOutPendingEvents(ctx context.Context, limit int) (eventCount, deliveryCount int, err error) {
	if limit <= 0 {
		limit = 200
	}
	tx, err := d.BeginTx(ctx, nil)
	if err != nil {
		return 0, 0, err
	}
	defer tx.Rollback() //nolint:errcheck // A rollback after commit is a no-op.

	channels, err := listEnabledNotificationChannelsTx(ctx, tx)
	if err != nil {
		return 0, 0, err
	}
	rows, err := tx.QueryContext(ctx, `SELECT id, kind, snapshot FROM notification_events
WHERE NOT fanned_out ORDER BY id FOR UPDATE SKIP LOCKED LIMIT $1`, limit)
	if err != nil {
		return 0, 0, err
	}
	type pendingEvent struct {
		id       int64
		snapshot notify.Snapshot
	}
	var events []pendingEvent
	for rows.Next() {
		var ev pendingEvent
		var kind string
		var raw json.RawMessage
		if err := rows.Scan(&ev.id, &kind, &raw); err != nil {
			rows.Close()
			return 0, 0, err
		}
		// Keep dispatching if a stored snapshot fails to decode. Channels with filters may skip that
		// event.
		_ = json.Unmarshal(raw, &ev.snapshot)
		// Use the row's kind as authoritative over legacy snapshot values.
		ev.snapshot.Kind = kind
		events = append(events, ev)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return 0, 0, err
	}
	if len(events) == 0 {
		return 0, 0, tx.Commit()
	}

	type pending struct {
		eventID   int64
		channelID int64
	}
	var toInsert []pending
	for _, ev := range events {
		for _, ch := range channels {
			if !notify.Match(ch.filter, ev.snapshot) {
				continue
			}
			toInsert = append(toInsert, pending{eventID: ev.id, channelID: ch.id})
		}
	}
	if len(toInsert) > 0 {
		var (
			vals []string
			args []any
		)
		for _, p := range toInsert {
			vals = append(vals, fmt.Sprintf("($%d,$%d)", len(args)+1, len(args)+2))
			args = append(args, p.eventID, p.channelID)
		}
		if _, err := tx.ExecContext(ctx, `INSERT INTO notification_deliveries(event_id,channel_id) VALUES `+strings.Join(vals, ","), args...); err != nil {
			return 0, 0, err
		}
	}

	// Mark all processed events fanned out, even if no channel matched.
	ids := make([]string, 0, len(events))
	markArgs := make([]any, 0, len(events))
	for _, ev := range events {
		markArgs = append(markArgs, ev.id)
		ids = append(ids, fmt.Sprintf("$%d", len(markArgs)))
	}
	if _, err := tx.ExecContext(ctx, `UPDATE notification_events SET fanned_out=true WHERE id IN (`+strings.Join(ids, ",")+`)`, markArgs...); err != nil {
		return 0, 0, err
	}
	return len(events), len(toInsert), tx.Commit()
}

type notificationRoute struct {
	id     int64
	filter notify.Filter
}

// Load current filters in the dispatch transaction and parse each once per batch.
func listEnabledNotificationChannelsTx(ctx context.Context, tx *sql.Tx) ([]notificationRoute, error) {
	rows, err := tx.QueryContext(ctx, `SELECT id, filter FROM notification_channels
WHERE enabled AND kind=$1 ORDER BY id`, notify.KindTelegram)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []notificationRoute
	for rows.Next() {
		var route notificationRoute
		var raw json.RawMessage
		if err := rows.Scan(&route.id, &raw); err != nil {
			return nil, err
		}
		route.filter = notify.ParseFilter(raw)
		out = append(out, route)
	}
	return out, rows.Err()
}

// NotificationAssetNames resolves asset IDs to brief display names.
//
// Preserve input order and skip missing IDs so delivery retry does not change the displayed
// asset order.
func (d *DB) NotificationAssetNames(ctx context.Context, ids []int64) ([]string, error) {
	if len(ids) == 0 {
		return nil, nil
	}
	ph, args := placeholders(1, ids)
	rows, err := d.QueryContext(ctx, `SELECT id, type, domain, ip, url, app_name, bundle_id FROM assets WHERE id IN (`+ph+`)`, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	labels := map[int64]string{}
	for rows.Next() {
		var (
			id                int64
			typ               string
			domain, ip, url   sql.NullString
			appName, bundleID sql.NullString
		)
		if err := rows.Scan(&id, &typ, &domain, &ip, &url, &appName, &bundleID); err != nil {
			return nil, err
		}
		labels[id] = assetDisplayName(typ, domain.String, ip.String, url.String, appName.String, bundleID.String)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	out := make([]string, 0, len(ids))
	seen := map[int64]bool{}
	for _, id := range ids {
		if seen[id] {
			continue
		}
		seen[id] = true
		if label, ok := labels[id]; ok && label != "" {
			out = append(out, label)
		}
	}
	return out, nil
}

// assetDisplayName chooses a recognizable label by asset type. The caller decides how to display
// assets with no usable label.
func assetDisplayName(typ, domain, ip, url, appName, bundleID string) string {
	pick := func(vals ...string) string {
		for _, v := range vals {
			if strings.TrimSpace(v) != "" {
				return v
			}
		}
		return ""
	}
	switch typ {
	case "root_domain", "subdomain":
		return domain
	case "ip":
		return ip
	case "app":
		return pick(appName, bundleID)
	case "service", "endpoint":
		return pick(url, domain, ip)
	default:
		return pick(domain, ip, url, appName)
	}
}

// SetFindingStatusWithNotify updates the status and queues a change event in one transaction.
//
// Return the previous status, whether the finding exists, and whether the event was queued.
//
// Unchanged or missing findings produce no event. Missing findings return found=false. An event
// insert failure reports notified=false while leaving the status update intact.
func (d *DB) SetFindingStatusWithNotify(ctx context.Context, id int64, status string) (from string, found bool, notified bool, err error) {
	err = d.WithEvidenceTx(ctx, func(tx *sql.Tx) error {
		var txErr error
		from, found, _, notified, txErr = SetFindingStatusTx(ctx, tx, id, status)
		return txErr
	})
	return from, found, notified, err
}

// SetFindingStatusTx updates the status and queues a change event in the caller's transaction.
//
// Use the same transaction-level update for retests and manual changes so on_status_change
// receives both.
//
// Report the previous status, existence, whether it changed, and whether the event was queued.
// Event failures do not abort the status update.
func SetFindingStatusTx(ctx context.Context, tx *sql.Tx, id int64, status string) (from string, found bool, changed bool, notified bool, err error) {
	var (
		vulnclass, name, severity, summary string
		taskID                             sql.NullInt64
		assetIDs                           []byte
	)
	scanErr := tx.QueryRowContext(ctx, `SELECT vulnclass, name, severity, summary, task_id, asset_ids, status
FROM findings WHERE id=$1 FOR UPDATE`, id).
		Scan(&vulnclass, &name, &severity, &summary, &taskID, &assetIDs, &from)
	if scanErr == sql.ErrNoRows {
		return "", false, false, false, nil
	}
	if scanErr != nil {
		return "", false, false, false, scanErr
	}
	found = true
	if from == status {
		// Do not queue an event for an unchanged status, including idempotent replays.
		return from, true, false, false, nil
	}
	if _, err := tx.ExecContext(ctx, `UPDATE findings SET status=$2 WHERE id=$1`, id, status); err != nil {
		return from, true, false, false, err
	}
	var assets []int64
	_ = json.Unmarshal(assetIDs, &assets)
	notified = RecordNotificationEventTx(ctx, tx, notify.EventFindingStatusChanged, id, notify.Snapshot{
		Kind:       notify.EventFindingStatusChanged,
		FindingID:  id,
		TaskID:     taskID.Int64,
		VulnClass:  vulnclass,
		Name:       name,
		Severity:   severity,
		Summary:    summary,
		AssetIDs:   assets,
		FromStatus: from,
		ToStatus:   status,
	})
	return from, true, true, notified, nil
}

// NotificationStats contains overview counts for the notification page.
type NotificationStats struct {
	Channels     int   `json:"channels"`
	ChannelsOn   int   `json:"channels_on"`
	Pending      int   `json:"pending"`
	Failed       int   `json:"failed"`
	SentToday    int   `json:"sent_today"`
	BacklogAgeMS int64 `json:"backlog_age_ms"` // Age of the oldest pending delivery, in milliseconds.
}

// NotificationStatsSnapshot reports queue health. Backlog age shows whether delivery is stalled
// even when only a few items are pending.
func (d *DB) NotificationStatsSnapshot(ctx context.Context) (*NotificationStats, error) {
	var s NotificationStats
	if err := d.QueryRowContext(ctx, `WITH channels AS (SELECT id, enabled FROM notification_channels WHERE kind=$1)
SELECT
    (SELECT count(*) FROM channels),
    (SELECT count(*) FROM channels WHERE enabled),
    count(*) FILTER (WHERE d.state IN ($2,$3)),
    count(*) FILTER (WHERE d.state=$4),
    count(*) FILTER (WHERE d.state=$5 AND d.sent_at >= date_trunc('day', now())),
    COALESCE(EXTRACT(EPOCH FROM (now() - min(d.created_at) FILTER (WHERE d.state=$2))) * 1000, 0)::bigint
FROM notification_deliveries d JOIN channels c ON c.id=d.channel_id`,
		notify.KindTelegram, NotifyStatePending, NotifyStateSending, NotifyStateFailed, NotifyStateSent).
		Scan(&s.Channels, &s.ChannelsOn, &s.Pending, &s.Failed, &s.SentToday, &s.BacklogAgeMS); err != nil {
		return nil, err
	}
	return &s, nil
}
