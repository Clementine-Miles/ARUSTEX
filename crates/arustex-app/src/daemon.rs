//! Daemon bootstrap: wires configuration, storage, providers, the scheduler, the IPC
//! server and the notifier together, and shuts them down on SIGINT/SIGTERM.

use std::sync::Arc;
use std::time::Duration;

use arustex_core::domain::agent::AgentDefinition;
use arustex_core::domain::record::IngestLimits;
use arustex_core::events::DomainEvent;
use arustex_core::ipc::SettingsView;
use arustex_core::{CoreError, CoreResult};
use arustex_llm::{ProviderRouter, ProviderSettings, build_provider};
use arustex_operations::Registry;
use arustex_policy::Policy;
use arustex_runtime::{AgentRunner, Scheduler, SchedulerSettings, TaskHooks, builtin_agents};
use arustex_store::repository::records::IngestOptions;
use arustex_store::{Store, StoreOptions};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Loaded, ProviderKind};
use crate::notify::{NotifyHooks, run_notifier};
use crate::server::{Server, ServerSettings};
use crate::services::Services;

/// Interval of the retention and garbage-collection pass.
const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);

pub fn store_options(cfg: &Config) -> StoreOptions {
    StoreOptions {
        max_connections: cfg.database.max_connections,
        acquire_timeout: Duration::from_secs(cfg.database.acquire_timeout_secs),
        statement_timeout: cfg.statement_timeout(),
        event_buffer: cfg.daemon.event_buffer,
    }
}

pub fn ingest_options(cfg: &Config, source: &str, redact: bool) -> IngestOptions {
    IngestOptions {
        source: source.to_owned(),
        redact,
        limits: IngestLimits {
            max_line_bytes: cfg.limits.max_record_line_bytes,
            max_body_bytes: cfg.limits.max_record_body_bytes,
            max_records: cfg.limits.max_records_per_import,
            ..IngestLimits::default()
        },
    }
}

/// Connects to PostgreSQL with the configured pool settings.
pub async fn connect_store(loaded: &Loaded) -> CoreResult<Store> {
    Store::connect(loaded.database_url.expose(), &store_options(&loaded.config)).await
}

/// Builds every configured provider. Only these endpoints are ever contacted.
pub fn build_router(loaded: &Loaded) -> CoreResult<ProviderRouter> {
    let mut router = ProviderRouter::new();
    for p in &loaded.config.providers {
        let key = loaded
            .provider_keys
            .iter()
            .find(|(name, _)| name == &p.name)
            .and_then(|(_, k)| k.clone());
        let kind = match p.kind {
            ProviderKind::OpenaiChat => arustex_llm::ProviderKind::OpenAiChat,
            ProviderKind::OpenaiResponses => arustex_llm::ProviderKind::OpenAiResponses,
            ProviderKind::Anthropic => arustex_llm::ProviderKind::Anthropic,
            ProviderKind::Mock => arustex_llm::ProviderKind::Mock,
        };
        let provider = build_provider(ProviderSettings {
            name: p.name.clone(),
            kind,
            model: p.model.clone(),
            base_url: p.effective_base_url(),
            api_key: key,
            timeout: Duration::from_secs(p.timeout_secs),
            connect_timeout: Duration::from_secs(p.connect_timeout_secs),
            max_retries: p.max_retries,
            requests_per_minute: p.requests_per_minute,
            context_window: p.context_window,
        })
        .map_err(CoreError::from)?;
        router.insert(provider, p.default);
    }
    Ok(router)
}

/// The built-in agents with operator overrides applied.
pub fn configured_agents(cfg: &Config) -> Vec<AgentDefinition> {
    builtin_agents()
        .into_iter()
        .map(|mut agent| {
            if let Some(o) = cfg.agents.iter().find(|o| o.key == agent.key) {
                if let Some(enabled) = o.enabled {
                    agent.enabled = enabled;
                }
                if let Some(profile) = &o.model_profile {
                    agent.model_profile = Some(profile.clone());
                }
                if let Some(turns) = o.max_turns {
                    agent.max_turns = turns;
                }
                if let Some(budget) = o.token_budget {
                    agent.token_budget = budget;
                }
            }
            agent
        })
        .collect()
}

/// The output-token cap handed to providers: the smallest configured value.
fn max_output_tokens(cfg: &Config) -> u32 {
    cfg.providers
        .iter()
        .map(|p| p.max_output_tokens)
        .min()
        .unwrap_or(4096)
}

pub fn scheduler_settings(cfg: &Config) -> SchedulerSettings {
    SchedulerSettings {
        poll_interval: Duration::from_millis(cfg.scheduler.poll_interval_ms),
        lease: Duration::from_secs(cfg.scheduler.lease_secs),
        max_concurrent_sessions: cfg.scheduler.max_concurrent_sessions,
        max_attempts: cfg.scheduler.max_attempts,
        planner_enabled: cfg.scheduler.planner_enabled,
        reviewer_enabled: cfg.scheduler.reviewer_enabled,
        ..SchedulerSettings::default()
    }
}

/// Everything the daemon runs, assembled but not yet started.
#[derive(Debug)]
pub struct Daemon {
    pub store: Arc<Store>,
    pub scheduler: Option<Arc<Scheduler>>,
    pub services: Arc<Services>,
    pub server: Server,
    loaded: Loaded,
    shutdown: CancellationToken,
}

impl Daemon {
    /// Builds the daemon. Refuses to start against an unmigrated database.
    pub async fn build(loaded: Loaded, shutdown: CancellationToken) -> CoreResult<Self> {
        let store = Arc::new(connect_store(&loaded).await?);
        store.ensure_migrated().await?;
        let cfg = &loaded.config;
        let router = build_router(&loaded)?;
        let agents = configured_agents(cfg);
        for agent in agents.iter().filter(|a| a.enabled) {
            router
                .get(agent.model_profile.as_deref())
                .map_err(|e| CoreError::invalid(format!("agent {}: {e}", agent.key)))?;
        }
        let registry = Arc::new(Registry::standard(Policy::standard()));
        let hooks: Arc<dyn TaskHooks> =
            Arc::new(NotifyHooks::new(store.clone(), cfg.telegram.clone()));
        let scheduler = if cfg.scheduler.enabled {
            let runner = AgentRunner::new(
                store.clone(),
                store.clone(),
                store.clone(),
                registry,
                router,
            )
            .with_max_output_tokens(max_output_tokens(cfg));
            let mut scheduler = Scheduler::new(store.clone(), runner, scheduler_settings(cfg))
                .with_hooks(hooks.clone());
            for agent in agents {
                scheduler = scheduler.with_agent(agent);
            }
            Some(Arc::new(scheduler))
        } else {
            None
        };
        let settings = SettingsView {
            entries: cfg.redacted_entries(),
        };
        let services = Arc::new(Services::new(
            store.clone(),
            scheduler.clone(),
            hooks,
            settings,
            shutdown.clone(),
        ));
        let server = Server::bind(ServerSettings {
            socket: loaded.socket.clone(),
            max_clients: cfg.daemon.max_clients,
            client_queue: cfg.daemon.client_queue,
            request_timeout: Duration::from_secs(cfg.daemon.request_timeout_secs),
        })
        .await?;
        Ok(Self {
            store,
            scheduler,
            services,
            server,
            loaded,
            shutdown,
        })
    }

    /// Runs until the shutdown token fires.
    pub async fn run(self) -> CoreResult<()> {
        let Self {
            store,
            scheduler,
            services,
            server,
            loaded,
            shutdown,
        } = self;
        store
            .record_event(
                None,
                None,
                DomainEvent::DaemonStarted {
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            )
            .await?;
        tracing::info!(socket = %server.socket().display(), "daemon listening");
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(server.serve(services, shutdown.clone()));
        tasks.spawn(run_notifier(
            store.clone(),
            loaded.config.telegram.clone(),
            loaded.telegram_token.clone(),
            shutdown.clone(),
        ));
        tasks.spawn(retention_loop(store.clone(), shutdown.clone()));
        let scheduler_task = scheduler.map(|s| {
            let token = shutdown.clone();
            tokio::spawn(async move {
                if let Err(e) = s.run(token).await {
                    tracing::error!(error = %e, "scheduler stopped");
                }
            })
        });
        shutdown.cancelled().await;
        tracing::info!("shutting down");
        if let Some(t) = scheduler_task {
            let _ = t.await;
        }
        while tasks.join_next().await.is_some() {}
        store.close().await;
        Ok(())
    }
}

async fn retention_loop(store: Arc<Store>, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                match store.purge_expired_records().await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(records = n, "purged expired records"),
                    Err(e) => tracing::warn!(error = %e, "record retention pass failed"),
                }
                match store.collect_garbage().await {
                    Ok((0, 0)) => {}
                    Ok((expired, orphans)) => tracing::info!(expired, orphans, "removed attachments"),
                    Err(e) => tracing::warn!(error = %e, "attachment collection failed"),
                }
            }
        }
    }
}

/// Cancels `token` on SIGINT or SIGTERM.
pub fn install_signal_handlers(token: CancellationToken) -> CoreResult<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())
        .map_err(|e| CoreError::Internal(format!("cannot install SIGTERM handler: {e}")))?;
    let mut int = signal(SignalKind::interrupt())
        .map_err(|e| CoreError::Internal(format!("cannot install SIGINT handler: {e}")))?;
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        token.cancel();
    });
    Ok(())
}
