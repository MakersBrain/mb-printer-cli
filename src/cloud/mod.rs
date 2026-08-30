// SPDX-License-Identifier: AGPL-3.0-or-later
//! Versioned cloud printer-agent contract and durable local state.

pub const PROTOCOL_VERSION: u32 = 1;

pub mod wire {
    tonic::include_proto!("makersbrain.print.agent.v1");
}

pub mod agent;
pub mod store;
