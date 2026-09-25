//! Assistant Gateway: runs arbitrary `sh` commands on the device host over
//! RPC (`exec` on `dv-assistant-gateway`).

pub mod app;
pub mod config;
pub mod diag;
pub mod executor;
pub mod parse;
pub mod tags;
