//! Telegram delivery, the only notifier.
//!
//! Notifications are written to a durable outbox and sent by one loop to the single
//! operator-configured chat. Workflow agents cannot enqueue notifications or choose a
//! destination; texts contain task names and statuses only, never record contents.

use std::sync::Arc;
use std::time::Duration;

use arustex_core::Secret;
use arustex_core::domain::task::{Task, TaskStatus};
use arustex_core::events::DomainEvent;
use arustex_runtime::TaskHooks;
use arustex_store::Store;
use async_trait::async_trait;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::config::{NotifyEvent, TelegramConfig};
use crate::reports;

/// Maximum characters of a task name quoted in a notification.
const MAX_NAME_CHARS: usize = 80;

/// The notification text for a finished task, if the operator enabled that event.
pub fn task_text(cfg: &TelegramConfig, task: &Task, report_id: Option<i64>) -> Option<String> {
    let event = match task.status {
        TaskStatus::Completed => NotifyEvent::TaskCompleted,
        TaskStatus::Failed | TaskStatus::TimedOut => NotifyEvent::TaskFailed,
        TaskStatus::NeedsReview => NotifyEvent::TaskNeedsReview,
        _ => return None,
    };
    if !cfg.events.contains(&event) {
        return None;
    }
    let name: String = task.display_name().chars().take(MAX_NAME_CHARS).collect();
    let mut text = format!("ARUSTEX task #{} \"{}\": {}", task.id, name, task.status);
    if let Some(id) = report_id {
        text.push_str(&format!(" (report #{id})"));
    }
    Some(text)
}

/// Generates a report and enqueues a status notification when a task finishes.
#[derive(Debug)]
pub struct NotifyHooks {
    store: Arc<Store>,
    telegram: TelegramConfig,
}

impl NotifyHooks {
    pub fn new(store: Arc<Store>, telegram: TelegramConfig) -> Self {
        Self { store, telegram }
    }
}

#[async_trait]
impl TaskHooks for NotifyHooks {
    async fn task_finished(&self, task: &Task) {
        let report_id = match reports::generate_report(&self.store, task.id).await {
            Ok(r) => Some(r.id.0),
            Err(e) => {
                tracing::warn!(task = %task.id, error = %e, "report generation failed");
                None
            }
        };
        if !self.telegram.enabled {
            return;
        }
        if let Some(text) = task_text(&self.telegram, task, report_id)
            && let Err(e) = self
                .store
                .enqueue_notification("task", Some(task.id), &text)
                .await
        {
            tracing::warn!(task = %task.id, error = %e, "could not enqueue notification");
        }
    }
}

/// Sends queued notifications to the configured chat.
#[derive(Debug)]
pub struct TelegramSender {
    client: reqwest::Client,
    url: String,
    chat_id: i64,
}

impl TelegramSender {
    pub fn new(cfg: &TelegramConfig, token: &Secret) -> Result<Self, String> {
        let chat_id = cfg
            .chat_id
            .ok_or_else(|| "telegram.chat_id is required when enabled".to_owned())?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let base = cfg.api_base.trim_end_matches('/');
        Ok(Self {
            client,
            url: format!("{base}/bot{}/sendMessage", token.expose()),
            chat_id,
        })
    }

    /// Sends one message. Errors never contain the bot token.
    pub async fn send(&self, text: &str) -> Result<(), String> {
        let response = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "request timed out".to_owned()
                } else if e.is_connect() {
                    "connection failed".to_owned()
                } else {
                    "request failed".to_owned()
                }
            })?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("telegram returned HTTP {}", status.as_u16()))
        }
    }
}

/// Runs the outbox loop until `shutdown`. When delivery is disabled, pending entries
/// are marked skipped and the loop only watches for approval requests to ignore.
pub async fn run_notifier(
    store: Arc<Store>,
    cfg: TelegramConfig,
    token: Option<Secret>,
    shutdown: CancellationToken,
) {
    let sender = match (&token, cfg.enabled) {
        (Some(token), true) => match TelegramSender::new(&cfg, token) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(error = %e, "telegram disabled");
                None
            }
        },
        _ => None,
    };
    let Some(sender) = sender else {
        match store.skip_pending_notifications().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                skipped = n,
                "telegram disabled; skipped pending notifications"
            ),
            Err(e) => tracing::warn!(error = %e, "could not skip pending notifications"),
        }
        return;
    };
    let interval = Duration::from_secs(cfg.min_interval_secs.max(1));
    let mut live = store.subscribe_live();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            event = live.recv() => match event {
                Ok(env) => {
                    if let DomainEvent::ApprovalRequested { approval_id, operation } = &env.event
                        && cfg.events.contains(&NotifyEvent::ApprovalRequested)
                    {
                        let op: String = operation.chars().take(64).collect();
                        let text = format!("ARUSTEX approval #{approval_id} requested for {op}");
                        if let Err(e) = store.enqueue_notification("approval", env.task_id, &text).await {
                            tracing::warn!(error = %e, "could not enqueue notification");
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            _ = ticker.tick() => {
                deliver_one(&store, &sender, cfg.max_attempts).await;
            }
        }
    }
}

/// Claims and sends at most one notification, honouring the minimum interval.
pub async fn deliver_one(store: &Store, sender: &TelegramSender, max_attempts: u32) -> bool {
    let batch = match store.claim_notifications(1).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "could not claim notifications");
            return false;
        }
    };
    let Some(n) = batch.into_iter().next() else {
        return false;
    };
    match sender.send(&n.text).await {
        Ok(()) => {
            if let Err(e) = store.notification_sent(n.id).await {
                tracing::warn!(error = %e, "could not mark notification sent");
            }
        }
        Err(err) => {
            tracing::warn!(id = n.id, attempts = n.attempts, error = %err, "telegram delivery failed");
            if let Err(e) = store.notification_failed(n.id, &err, max_attempts).await {
                tracing::warn!(error = %e, "could not record notification failure");
            }
        }
    }
    true
}
