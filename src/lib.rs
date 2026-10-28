//! Shared research engine for the CLI and library consumers.
//!
//! The core does not invoke Python, Node, jq, shell commands, or platform UI APIs.
//! Optional browser execution uses a locally installed Chrome/Chromium through CDP.

pub mod browser;
pub mod extract;
pub mod fetch;
pub mod model;
pub mod net;
pub mod providers;
pub mod query;
pub mod store;

pub mod discovery;

mod engines;

pub mod sampling;

mod scholarly;

pub mod research;

pub mod reddit;

pub mod youtube;

pub mod arxiv;
