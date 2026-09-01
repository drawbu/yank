//! `yank` shares one clipboard across the machines you own.

#![warn(clippy::pedantic)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

pub mod cli;
pub mod clip;
pub mod config;
pub mod daemon;
pub mod log;
pub mod net;
