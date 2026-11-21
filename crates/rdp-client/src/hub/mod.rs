//! # Hub — saved connections and connection history
//!
//! The stores the launcher, the connection bar and the session share, under
//! `%LOCALAPPDATA%\rdpio\`:
//!
//! - `connections.json`: saved connections ([`ConnectionStore`]), passwords
//!   DPAPI-protected ([`ProtectedPassword`]);
//! - `history.json`: the most recently used connections ([`MruStore`]);
//! - `settings.json`: the keep-open preference ([`store::Settings`]).
//!
//! The launcher keeps its per-connection options in `launcher.json`
//! (`launcher::profile`), keyed by the same connection id.

mod error;

// Records, the DPAPI-protected password and the history entry. Some helpers
// are only used by the tests.
#[allow(dead_code)]
pub mod model;
// The JSON persistence layer: ConnectionStore, MruStore and the settings.
#[allow(dead_code)]
pub mod store;

pub use model::{ConnectionRecord, ProtectedPassword};
pub use store::{ConnectionInput, ConnectionStore, MruStore};

// Error type shared by every fallible store operation.
pub use error::HubError;

// Thin re-export so hub submodules (model, store) reach DPAPI through one path;
// the functions are already `pub(crate)` in token_cache.rs.
pub(crate) use crate::token_cache::{dpapi_protect, dpapi_unprotect};
