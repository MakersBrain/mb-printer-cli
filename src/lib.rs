// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native command and secure loopback service.

pub mod api;
pub mod assets;
pub mod auth;
pub mod cli;
pub mod config;
pub mod device;
pub mod jobs;
pub mod laposte;
pub mod raster;
pub mod transport;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
