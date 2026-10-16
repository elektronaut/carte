//! The reverse proxy: accepts HTTP(S) on loopback, asks a [`Router`] where each
//! request goes, and forwards it to an app on `127.0.0.1` or `::1`, including
//! WebSocket upgrades.

mod error_page;

pub use error_page::ErrorPage;
