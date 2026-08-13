//! rv64-core: portable RV64 CPU core.
//!
//! The CPU is generic over a [`Bus`]. The production full-system machine uses
//! this boundary for RAM, MMIO, and interrupt delivery. A flat implementation
//! supports architecture tests without creating a second emulator.

#![cfg_attr(not(test), no_std)]

pub mod bus;
pub mod compressed;
pub mod cpu;
pub mod csr;
pub mod decode;
pub mod exception;
pub mod softfp;

pub use bus::{Bus, FlatMemory};
pub use cpu::{Cpu, StopReason};
pub use exception::Exception;
