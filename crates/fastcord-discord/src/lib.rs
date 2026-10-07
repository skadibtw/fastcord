//! Authenticated Discord control-plane I/O. One [`RestClient`] belongs to one account.
//! All clones share its rate limits and permanent authentication-stop signal.
//! The main user [`Gateway`] connection lifecycle lives in [`gateway`]; the
//! bounded reducer state it feeds lives in [`state`]. Channel history is read
//! through [`history`], and the bounded cache of message bodies the timeline
//! reads from is [`message_store`].

mod clock;
mod error;
pub mod gateway;
pub mod history;
pub mod message_store;
mod rate_limit;
mod remote_auth;
mod rest;
mod route;
mod secret;
pub mod state;
mod ws;

pub use clock::{Clock, MonotonicClock};
pub use error::{NetworkFailure, RestError, RetryableFailure};
pub use gateway::{ConnectionState, Gateway, GatewayEvent};
pub use remote_auth::{QrLink, RemoteAuth, RemoteAuthError, RemoteAuthEvent, RemoteUser};
pub use reqwest::{Method, StatusCode};
pub use rest::{Priority, RestClient, RestRequest, RestResponse};
pub use route::{MajorParameter, Route, RouteKey};
pub use secret::UserToken;
