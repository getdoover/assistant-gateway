//! Assistant Gateway: runs arbitrary `sh` commands on the device host over
//! RPC (`exec` on `dv-assistant-gateway`), plus typed network diagnostics,
//! checkpointed network changes and Modbus access.

pub mod app;
pub mod config;
pub mod diag;
pub mod executor;
pub mod modbus;
pub mod netapply;
pub mod parse;
pub mod tags;
