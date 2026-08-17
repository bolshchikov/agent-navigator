use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use crate::client::{AgentNavigator, NavigateRequest};
use crate::config::{parse_tier, ClientConfig, OverrideFile};
use crate::envelope::FetchEnvelope;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "agent-navigator",
    version,
    about = "Agent-native HTTP client: fetch, classify, extract, and discover structured web surfaces. Never executes JavaScript."
)]
pub struct Cli {
    /// Named cookie session.
    #[arg(long, global = true, default_value = "default")]
    pub session: String,

    /// Explicit opt-in to ignore robots.txt. Use only with other authorization.
    #[arg(long, global = true)]
    pub ignore_robots: bool,

    /// Include raw HTML in the envelope.
    #[arg(long, global = true)]
    pub include_html: bool,

    /// Override capability tier: structured | static-readable | js-required
    #[arg(long, global = true)]
    pub force_tier: Option<String>,

    /// JSON file of hostname → tier overrides.
    #[arg(long, global = true)]
    pub overrides: Option<PathBuf>,

    /// Directory for persisted cookie jars.
    #[arg(long, global = true)]
    pub session_dir: Option<PathBuf>,

    /// Compact JSON instead of pretty-printed.
    #[arg(long, global = true)]
    pub compact: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Fetch a URL and return the typed envelope.
    Navigate {
        url: String,
        #[arg(long, default_value = "GET")]
        method: String,
        /// Repeatable Header: Value
        #[arg(long = "header")]
        headers: Vec<String>,
        /// JSON body for POST/PUT/PATCH
        #[arg(long)]
        body: Option<String>,
    },
    /// Alias of navigate focusing on extracted content.
    Extract { url: String },
    /// Discover llms.txt / JSON-LD / WebMCP without returning markdown.
    Discover { url: String },
    /// Invoke a declarative WebMCP tool via HTTP.
    CallTool {
        url: String,
        name: String,
        /// JSON object of arguments
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Submit a static HTML form.
    SubmitForm {
        url: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "{}")]
        fields: String,
    },
    /// Invoke a schema.org JSON-LD Action via its HTTP EntryPoint.
    CallJsonLdAction {
        url: String,
        name: String,
        /// JSON object matching the Action's input schema
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Start the MCP server on stdio.
    Mcp,
    /// Run the local fixture corpus (and optionally live URLs).
    Corpus {
        #[arg(long, default_value = "corpus/manifest.json")]
        manifest: PathBuf,
        #[arg(long)]
        live: bool,
    },
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = build_config(&cli)?;

    match cli.command {
        Command::Mcp => run_mcp(config).await,
        Command::Corpus { manifest, live } => crate::corpus::run(&manifest, live).await,
        other => {
            let client = AgentNavigator::new(config)?;
            let env = dispatch(
                &client,
                &cli.session,
                cli.ignore_robots,
                cli.include_html,
                cli.force_tier.as_deref(),
                other,
            )
            .await;
            print_envelope(&env, cli.compact)?;
            if matches!(env.status, crate::envelope::EnvelopeStatus::Error { .. }) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn build_config(cli: &Cli) -> anyhow::Result<ClientConfig> {
    let tier_overrides = if let Some(path) = &cli.overrides {
        let text = std::fs::read_to_string(path)?;
        let file: OverrideFile = serde_json::from_str(&text)?;
        file.into_tiers()
    } else {
        Default::default()
    };
    Ok(ClientConfig {
        ignore_robots: cli.ignore_robots,
        session_dir: cli
            .session_dir
            .clone()
            .unwrap_or_else(crate::config::default_session_dir),
        tier_overrides,
        ..ClientConfig::default()
    })
}

async fn dispatch(
    client: &AgentNavigator,
    session: &str,
    ignore_robots: bool,
    include_html: bool,
    force_tier: Option<&str>,
    command: Command,
) -> FetchEnvelope {
    let force_tier = force_tier.and_then(parse_tier);
    match command {
        Command::Navigate {
            url,
            method,
            headers,
            body,
        } => {
            let body = match body {
                None => None,
                Some(s) => match serde_json::from_str(&s) {
                    Ok(v) => Some(v),
                    Err(err) => return invalid_json(&url, session, "body", err),
                },
            };
            client
                .navigate(NavigateRequest {
                    url,
                    method: Some(method),
                    session: Some(session.to_string()),
                    headers: parse_headers(&headers),
                    body,
                    include_html,
                    ignore_robots,
                    force_tier,
                    ..Default::default()
                })
                .await
        }
        Command::Extract { url } => {
            client
                .extract(NavigateRequest {
                    url,
                    session: Some(session.to_string()),
                    include_html,
                    ignore_robots,
                    force_tier,
                    ..Default::default()
                })
                .await
        }
        Command::Discover { url } => {
            client
                .discover(NavigateRequest {
                    url,
                    session: Some(session.to_string()),
                    include_html,
                    ignore_robots,
                    force_tier,
                    ..Default::default()
                })
                .await
        }
        Command::CallTool { url, name, args } => {
            let args = match serde_json::from_str(&args) {
                Ok(v) => v,
                Err(err) => return invalid_json(&url, session, "args", err),
            };
            client
                .call_webmcp_tool(&url, &name, args, Some(session.to_string()), ignore_robots)
                .await
        }
        Command::SubmitForm { url, name, fields } => {
            let fields = match serde_json::from_str(&fields) {
                Ok(v) => v,
                Err(err) => return invalid_json(&url, session, "fields", err),
            };
            client
                .submit_form(
                    &url,
                    name.as_deref(),
                    fields,
                    Some(session.to_string()),
                    ignore_robots,
                )
                .await
        }
        Command::CallJsonLdAction { url, name, args } => {
            let args = match serde_json::from_str(&args) {
                Ok(value) => value,
                Err(err) => return invalid_json(&url, session, "args", err),
            };
            client
                .call_jsonld_action(&url, &name, args, Some(session.to_string()), ignore_robots)
                .await
        }
        Command::Mcp | Command::Corpus { .. } => unreachable!(),
    }
}

fn parse_headers(headers: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for h in headers {
        if let Some((k, v)) = h.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn invalid_json(url: &str, session: &str, what: &str, err: serde_json::Error) -> FetchEnvelope {
    FetchEnvelope::error(
        url,
        "invalid_json",
        format!("invalid {what} JSON: {err}"),
        Duration::ZERO,
        crate::config::USER_AGENT,
        session,
    )
}

fn print_envelope(env: &FetchEnvelope, compact: bool) -> anyhow::Result<()> {
    if compact {
        println!("{}", serde_json::to_string(env)?);
    } else {
        println!("{}", serde_json::to_string_pretty(env)?);
    }
    Ok(())
}

async fn run_mcp(config: ClientConfig) -> anyhow::Result<()> {
    let client = std::sync::Arc::new(AgentNavigator::new(config)?);
    let server = crate::mcp::AgentNavigatorMcp::new(client);
    tracing::info!(
        "agent-navigator MCP server listening on stdio JSON-RPC (logs on stderr, never stdout)"
    );
    tracing::info!(
        "JSON-RPC tools: navigate, extract, discover, call_webmcp_tool, submit_form, call_jsonld_action"
    );
    tracing::info!(
        "declarative WebMCP is HTML <form toolname tooldescription>, invoked as HTTP by call_webmcp_tool — the site is not an MCP server"
    );
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
