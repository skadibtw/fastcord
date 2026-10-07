//! Authenticated Discord control-plane I/O. One [`RestClient`] belongs to one account.
//! All clones share its rate limits and permanent authentication-stop signal.

mod clock;
mod error;
mod rate_limit;
mod rest;
mod route;
mod secret;

pub use clock::{Clock, MonotonicClock};
pub use error::{NetworkFailure, RestError, RetryableFailure};
pub use reqwest::{Method, StatusCode};
pub use rest::{Priority, RestClient, RestRequest, RestResponse};
pub use route::{MajorParameter, Route, RouteKey};
pub use secret::UserToken;
