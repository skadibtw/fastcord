//! Discord domain types shared by every fastcord crate.
//!
//! This crate has no runtime, GUI, or I/O dependencies.

mod snowflake;

pub use snowflake::{ParseSnowflakeError, Snowflake};
