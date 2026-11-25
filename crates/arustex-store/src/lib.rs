//! PostgreSQL persistence. Every durable application record, transcript, audit event,
//! notification and attachment lives here; no other datastore is used.

pub mod attachments;
pub mod backup;
pub mod events;
pub mod legacy;
pub mod migration;
pub mod postgres;
pub mod repository;
pub mod testing;

pub use postgres::{Store, StoreOptions};
