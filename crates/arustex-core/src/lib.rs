//! Domain model, durable event types, repository ports and IPC contracts for ARUSTEX.
//!
//! This crate has no I/O. Every other workspace crate depends on it, and it depends on
//! none of them, which keeps the dependency graph acyclic.

pub mod domain;
pub mod error;
pub mod events;
pub mod ipc;
pub mod ports;

pub use error::{CoreError, CoreResult, ErrorCode};

/// Hard ceiling on the page size accepted by any listing call.
pub const MAX_PAGE_SIZE: u32 = 500;

/// Offset pagination used by listing calls and IPC requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub offset: u32,
    pub limit: u32,
}

impl Page {
    pub const fn new(offset: u32, limit: u32) -> Self {
        Self { offset, limit }
    }

    /// Returns a copy whose limit is clamped to `1..=MAX_PAGE_SIZE`.
    pub fn clamped(self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.clamp(1, MAX_PAGE_SIZE),
        }
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new(0, 50)
    }
}

/// One page of results plus the total number of matching rows.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: Page,
}

impl<T> Paged<T> {
    pub fn new(items: Vec<T>, total: u64, page: Page) -> Self {
        Self { items, total, page }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_limit_is_clamped() {
        assert_eq!(Page::new(0, 0).clamped().limit, 1);
        assert_eq!(Page::new(5, 10_000).clamped().limit, MAX_PAGE_SIZE);
        assert_eq!(Page::new(5, 20).clamped(), Page::new(5, 20));
    }
}
