#![allow(clippy::too_many_arguments)]

pub mod classify;
pub mod cli;
pub mod client;
pub mod config;
pub mod corpus;
pub mod discover;
pub mod envelope;
pub mod error;
pub mod extract;
pub mod http;
pub mod mcp;
pub mod mcp_http;
pub mod session;

pub use client::{AgentNavigator, NavigateRequest};
pub use config::{ClientConfig, USER_AGENT};
pub use envelope::{CapabilityTier, FetchEnvelope};

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
