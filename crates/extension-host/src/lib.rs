//! Platform-independent extension host shared by every Serein front end.
//!
//! The Wasm sandbox and wire contract live in the `extensions` crate. This crate owns what a
//! front end needs around it without depending on a windowing toolkit: the bounded package
//! store and worker ([`store`]), invocation snapshots built from [`client_core::State`]
//! ([`snapshot`]), and admission of app/data/message events ([`data_events`],
//! [`message_events`]). Device-local UI state reaches snapshots through [`HostLocal`].

pub mod data_events;
pub mod forum_data;
pub mod member_details;
pub mod message_content;
pub mod message_events;
pub mod snapshot;
pub mod store;

pub use snapshot::{HostLocal, RosterRow};
