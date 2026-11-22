//! Platform-independent extension host shared by every Serein front end.
//!
//! The Wasm sandbox and wire contract live in the `extensions` crate. This crate owns what a
//! front end needs around it without depending on a windowing toolkit: the bounded package
//! store and worker ([`store`]), invocation snapshots built from [`client_core::State`]
//! ([`snapshot`]), and admission of app/data/message events ([`data_events`],
//! [`message_events`]), each plugin's private SQLite file ([`database`]) and the account's message
//! journal ([`journal`]). Device-local UI state reaches snapshots through [`HostLocal`].

pub mod broker;
pub mod captions;
pub mod data_events;
pub mod database;
pub mod forum_data;
pub mod hooks;
pub mod journal;
pub mod member_details;
pub mod message_content;
pub mod message_events;
pub mod network;
pub mod notify;
pub mod secrets;
pub mod snapshot;
pub mod store;

pub use snapshot::{HostLocal, RosterRow};
