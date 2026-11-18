//! Route-sync service of Sakala Edge (ADR-019).
//!
//! Edge is a host Caddy plus this service. The control plane publishes which
//! nodes serve which project; `sakala-edge` polls that table, checks it,
//! renders a Caddyfile fragment, and has Caddy validate and reload it. The last
//! table Caddy accepted stays on disk, so an API outage or a restart never
//! drops routes, and an empty or invalid table is refused instead of served.

pub mod caddy;
pub mod config;
pub mod files;
pub mod render;
pub mod source;
pub mod sync;
pub mod table;
