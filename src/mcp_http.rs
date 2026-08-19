use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::client::AgentNavigator;
use crate::config::ClientConfig;
use crate::mcp::AgentNavigatorMcp;

const RATE_LIMIT: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_IPS: usize = 10_000;
const MAX_IN_FLIGHT: usize = 16;

struct RateLimiter {
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut guard = match self.hits.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        guard.retain(|_, times| times.iter().any(|t| now.duration_since(*t) < RATE_WINDOW));
        if !guard.contains_key(&ip) && guard.len() >= MAX_TRACKED_IPS {
            if let Some(oldest) = guard.keys().next().cloned() {
                guard.remove(&oldest);
            }
        }
        let times = guard.entry(ip).or_default();
        times.retain(|t| now.duration_since(*t) < RATE_WINDOW);
        if times.len() >= RATE_LIMIT {
            return false;
        }
        times.push(now);
        true
    }
}

#[derive(Clone)]
struct LimitState {
    limiter: Arc<RateLimiter>,
    inflight: Arc<Semaphore>,
}

pub struct HttpMcpOptions {
    pub bind: String,
    pub allowed_hosts: Vec<String>,
}

pub async fn run_http_mcp(config: ClientConfig, opts: HttpMcpOptions) -> anyhow::Result<()> {
    let config = config.public_http_demo();
    let inner = Arc::new(AgentNavigator::new(config)?);
    let hosts = opts.allowed_hosts.clone();

    let mcp = StreamableHttpService::new(
        {
            let inner = inner.clone();
            move || Ok(AgentNavigatorMcp::public_http(inner.clone()))
        },
        LocalSessionManager::default().into(),
        streamable_config(&hosts),
    );

    let limits = LimitState {
        limiter: Arc::new(RateLimiter::new()),
        inflight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };

    let mcp_router = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(limits, mcp_limits));

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index))
        .merge(mcp_router);

    let listener = tokio::net::TcpListener::bind(&opts.bind).await?;
    tracing::info!(bind = %opts.bind, hosts = ?hosts, "agent-navigator MCP server listening on Streamable HTTP at /mcp");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

fn streamable_config(hosts: &[String]) -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts.iter().cloned())
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "name": "agent-navigator",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn index() -> Json<serde_json::Value> {
    Json(json!({
        "name": "agent-navigator",
        "mcp": "/mcp",
        "health": "/health",
        "transport": "streamable-http",
        "warning": "Public demo. Do not send secrets. Loopback/private URLs, ignore_robots, include_html, and caller headers are disabled.",
    }))
}

async fn mcp_limits(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<LimitState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = client_ip(request.headers(), addr.ip());
    if !state.limiter.allow(ip) {
        tracing::warn!(%ip, "rate limited");
        return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
    }
    let _permit = if request.method() == Method::POST {
        match state.inflight.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return (StatusCode::TOO_MANY_REQUESTS, "busy").into_response(),
        }
    } else {
        None
    };
    next.run(request).await
}

fn client_ip(headers: &HeaderMap, fallback: IpAddr) -> IpAddr {
    // One trusted proxy (Render): the rightmost hop is the address the edge added.
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next_back())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(fallback)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

pub fn collect_allowed_hosts(cli_hosts: &[String], bind: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    hosts.extend(cli_hosts.iter().cloned());
    if let Ok(env) = std::env::var("MCP_ALLOWED_HOSTS") {
        hosts.extend(
            env.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    if let Ok(name) = std::env::var("RENDER_EXTERNAL_HOSTNAME") {
        if !name.is_empty() {
            hosts.push(name);
        }
    }
    if let Ok(url) = std::env::var("RENDER_EXTERNAL_URL") {
        if let Ok(parsed) = url::Url::parse(&url) {
            if let Some(host) = parsed.host_str() {
                hosts.push(host.to_string());
                if let Some(port) = parsed.port() {
                    hosts.push(format!("{host}:{port}"));
                }
            }
        }
    }
    let port = bind.rsplit(':').next().unwrap_or("10000");
    if bind_is_loopback(bind) {
        hosts.extend([
            "localhost".into(),
            "127.0.0.1".into(),
            "[::1]".into(),
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
        ]);
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

fn bind_is_loopback(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

pub fn default_bind(cli_bind: Option<&str>) -> String {
    if let Some(bind) = cli_bind {
        return bind.to_string();
    }
    let port = std::env::var("PORT").unwrap_or_else(|_| "10000".into());
    format!("0.0.0.0:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn public_bind_does_not_allow_localhost_host() {
        let hosts = collect_allowed_hosts(&["demo.onrender.com".into()], "0.0.0.0:10000");
        assert!(hosts.contains(&"demo.onrender.com".into()));
        assert!(!hosts
            .iter()
            .any(|h| h.contains("localhost") || h.starts_with("127.")));
    }

    #[test]
    fn loopback_bind_allows_localhost_host() {
        let hosts = collect_allowed_hosts(&[], "127.0.0.1:18765");
        assert!(hosts.contains(&"127.0.0.1:18765".into()));
        assert!(hosts.contains(&"localhost".into()));
    }

    #[test]
    fn client_ip_uses_rightmost_forwarded_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 8.8.8.8"),
        );
        let ip = client_ip(&headers, "9.9.9.9".parse().unwrap());
        assert_eq!(ip, "8.8.8.8".parse::<IpAddr>().unwrap());
    }
}
