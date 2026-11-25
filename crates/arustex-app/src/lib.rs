//! Composition root: configuration, application services, the daemon, the local IPC
//! server and client, record import, report generation and notifications.

pub mod client;
pub mod config;
pub mod daemon;
pub mod ingest;
pub mod notify;
pub mod reports;
pub mod server;
pub mod services;
