// Package notify formats and delivers finding notifications through Telegram.
package notify

const KindTelegram = "telegram"

// Event types correspond to notification_events.kind.
const (
	EventFindingCreated       = "finding_created"
	EventFindingStatusChanged = "finding_status_changed"
)

// Snapshot is the notification_events.snapshot JSONB payload written by
// finding transactions and read by the notification engine.
//
// Copy finding fields so routing and rendering reflect the event as recorded.
// Later changes must not alter earlier notifications.
type Snapshot struct {
	// Kind is finding_created or finding_status_changed.
	Kind      string  `json:"kind"`
	FindingID int64   `json:"finding_id"`
	TaskID    int64   `json:"task_id"`
	VulnClass string  `json:"vulnclass"`
	Name      string  `json:"name"`
	Severity  string  `json:"severity"`
	Summary   string  `json:"summary"`
	AssetIDs  []int64 `json:"asset_ids"`
	// Status fields are populated for finding_status_changed.
	FromStatus string `json:"from_status,omitempty"`
	ToStatus   string `json:"to_status,omitempty"`
}

// Item is a finding prepared for channel rendering.
type Item struct {
	FindingID int64
	Name      string
	VulnClass string
	Severity  string
	Summary   string
	// Assets contains display names such as domains or IPs, resolved by the server.
	Assets []string
	// DetailURL links to the finding and is empty when public_base_url is unset.
	DetailURL string
	// FromStatus and ToStatus describe the transition in a status notification.
	FromStatus string
	ToStatus   string
}

// IsStatusChange reports whether the item represents a status transition.
func (i Item) IsStatusChange() bool { return i.FromStatus != "" || i.ToStatus != "" }

// Title uses the saved name or finding class and falls back to a placeholder.
func (i Item) Title() string {
	if i.Name != "" {
		return i.Name
	}
	if i.VulnClass != "" {
		return i.VulnClass
	}
	return "(未命名漏洞)"
}

// Message is the full payload for one channel delivery.
type Message struct {
	// Items contains one finding for immediate delivery or several for a digest.
	// The caller must supply at least one item.
	Items []Item
	// Batch selects the digest title, time window, and count.
	Batch bool
	// WindowMinutes is the configured digest window. Supplying it explicitly keeps
	// rendering deterministic.
	WindowMinutes int
	// HomeURL links to the dashboard and is omitted when public_base_url is unset.
	HomeURL string
}
