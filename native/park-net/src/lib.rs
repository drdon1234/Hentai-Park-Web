mod connect;
pub mod chat;
mod jar;
mod model;
mod policy;
mod read_scope;
mod session;
mod transport;
mod web_player;
pub use policy::{authorize, public_address};
pub use read_scope::ReadScope;
pub use session::{Purpose, Session, Sessions};
pub use transport::{public_https, Request, Response, Transport, WireResponse};
pub use web_player::{GameNavigation, WebNavigation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("BLOCKED")]
    Blocked,
    #[error("TLS")]
    Tls,
    #[error("NETWORK")]
    Network,
    #[error("TIMEOUT")]
    Timeout,
    #[error("CANCELLED")]
    Cancelled,
    #[error("LIMIT")]
    Limit,
    #[error("STORAGE")]
    Storage,
}
