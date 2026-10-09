//! Authenticated Discord control-plane I/O. One [`RestClient`] belongs to one account.
//! All clones share its rate limits and permanent authentication-stop signal.
//! The main user [`Gateway`] connection lifecycle lives in [`gateway`]; the
//! bounded reducer state it feeds lives in [`state`]. Channel history is read
//! through [`history`], the bounded cache of message bodies the timeline
//! reads from is [`message_store`], and messages are created, never re-posted
//! by the transport, through [`RestClient::create_message`]. The account's own
//! messages are edited and deleted through [`RestClient::edit_message`] and
//! [`RestClient::delete_message`] (see [`is_own_message`]).

mod attachment;
mod clock;
mod edit;
mod error;
pub mod gateway;
pub mod history;
pub mod message_store;
mod private_channel;
mod rate_limit;
mod remote_auth;
mod rest;
mod route;
mod secret;
mod send;
pub mod state;
mod ws;

pub use attachment::RefreshedAttachmentUrl;
pub use clock::{Clock, MonotonicClock};
pub use edit::{EditedMessage, edit_response_update, is_own_message};
pub use error::{NetworkFailure, RestError, RetryableFailure};
pub use gateway::{ConnectionState, Gateway, GatewayEvent};
pub use private_channel::{MAX_PRIVATE_RECIPIENTS, PrivateChannelError};
pub use remote_auth::{QrLink, RemoteAuth, RemoteAuthError, RemoteAuthEvent, RemoteUser};
pub use reqwest::{Method, StatusCode};
pub use rest::{Priority, RestClient, RestRequest, RestResponse};
pub use route::{MajorParameter, Route, RouteKey};
pub use secret::UserToken;
pub use send::{
    ComposeError, MAX_CONTENT_CHARS, MentionPolicy, Nonce, NonceGenerator, OutgoingMessage,
    ReplyTo, SendError,
};
