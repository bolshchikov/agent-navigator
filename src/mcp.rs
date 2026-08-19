use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::client::{AgentNavigator, NavigateRequest};
use crate::config::parse_tier;
use crate::envelope::{EnvelopeStatus, FetchEnvelope, ToolInvocation};
use crate::session::{namespaced_session, validate_session_name};

#[derive(Clone)]
pub struct AgentNavigatorMcp {
    inner: Arc<AgentNavigator>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    /// Public Streamable HTTP demo: isolate cookies, drop caller headers, force robots.
    public: bool,
    fallback_namespace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NavigateParams {
    #[schemars(description = "HTTP URL to fetch")]
    pub url: String,
    #[schemars(description = "Named cookie session (default: default)")]
    pub session: Option<String>,
    #[schemars(description = "Include raw HTML in content.html")]
    pub include_html: Option<bool>,
    #[schemars(
        description = "Explicit opt-in to ignore robots.txt. Default false. Do not use unless you have other authorization."
    )]
    pub ignore_robots: Option<bool>,
    #[schemars(
        description = "Override capability classification: structured | static-readable | js-required"
    )]
    pub force_tier: Option<String>,
    #[schemars(
        description = "Optional Authorization or other headers. User-Agent is ignored — AgentNavigator always self-identifies."
    )]
    pub headers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallToolParams {
    #[schemars(description = "Page URL that declares the declarative WebMCP tool")]
    pub url: String,
    #[schemars(description = "toolname of the declarative WebMCP tool")]
    pub name: String,
    #[schemars(description = "JSON object of form field values")]
    pub arguments: Option<serde_json::Value>,
    pub session: Option<String>,
    pub ignore_robots: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitFormParams {
    pub url: String,
    #[schemars(
        description = "form id, name, or WebMCP toolname. If omitted, the first form is used."
    )]
    pub form: Option<String>,
    pub fields: Option<serde_json::Value>,
    pub session: Option<String>,
    pub ignore_robots: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JsonLdActionParams {
    #[schemars(description = "Page URL that declares the schema.org Action")]
    pub url: String,
    #[schemars(description = "Action name returned in the page envelope's tools_available")]
    pub name: String,
    #[schemars(description = "JSON object matching the Action's input_schema")]
    pub arguments: Option<serde_json::Value>,
    pub session: Option<String>,
    pub ignore_robots: Option<bool>,
}

impl AgentNavigatorMcp {
    pub fn new(inner: Arc<AgentNavigator>) -> Self {
        Self {
            inner,
            tool_router: Self::tool_router(),
            public: false,
            fallback_namespace: "local".into(),
        }
    }

    pub fn public_http(inner: Arc<AgentNavigator>) -> Self {
        Self {
            inner,
            tool_router: Self::tool_router(),
            public: true,
            fallback_namespace: random_namespace(),
        }
    }

    fn cookie_session(
        &self,
        ctx: &RequestContext<RoleServer>,
        requested: Option<String>,
    ) -> String {
        if !self.public {
            return requested
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "default".into());
        }
        let jar = requested
            .as_deref()
            .filter(|s| validate_session_name(s).is_ok())
            .unwrap_or("default");
        let ns = mcp_session_id(ctx).unwrap_or_else(|| self.fallback_namespace.clone());
        namespaced_session(&ns, jar)
    }

    fn ignore_robots(&self, requested: Option<bool>) -> bool {
        if self.public {
            false
        } else {
            requested.unwrap_or(false)
        }
    }
}

fn random_namespace() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mix = t ^ u64::from(std::process::id());
    format!("{mix:016x}")
}

fn mcp_session_id(ctx: &RequestContext<RoleServer>) -> Option<String> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tool_router]
impl AgentNavigatorMcp {
    #[tool(
        description = "Fetch a URL without executing JavaScript. Returns a typed envelope: status, capability_tier (structured | static-readable | js-required), markdown content, discovery metadata, and available tools. js-required is an escalation signal, not a silent empty page. WebMCP Imperative API is not supported."
    )]
    async fn navigate(
        &self,
        Parameters(params): Parameters<NavigateParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "navigate",
            self.inner.navigate(self.to_nav(params, &ctx)).await,
        )
    }

    #[tool(
        description = "Fetch a URL and return extracted markdown plus metadata. Same envelope as navigate."
    )]
    async fn extract(
        &self,
        Parameters(params): Parameters<NavigateParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "extract",
            self.inner.extract(self.to_nav(params, &ctx)).await,
        )
    }

    #[tool(
        description = "Discover agent-facing surfaces on a URL: llms.txt, JSON-LD Actions, OpenGraph, declarative WebMCP tools. Imperative WebMCP is detected and reported as unsupported."
    )]
    async fn discover(
        &self,
        Parameters(params): Parameters<NavigateParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "discover",
            self.inner.discover(self.to_nav(params, &ctx)).await,
        )
    }

    #[tool(
        description = "Invoke a declarative WebMCP tool by issuing the HTTP form request. toolautosubmit is treated as on even when the attribute is missing (no browser confirmation UI). Does not execute JavaScript. Tools registered only via navigator.modelContext.registerTool cannot be called."
    )]
    async fn call_webmcp_tool(
        &self,
        Parameters(params): Parameters<CallToolParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "call_webmcp_tool",
            self.inner
                .call_webmcp_tool(
                    &params.url,
                    &params.name,
                    params.arguments.unwrap_or(serde_json::json!({})),
                    Some(self.cookie_session(&ctx, params.session)),
                    self.ignore_robots(params.ignore_robots),
                )
                .await,
        )
    }

    #[tool(
        description = "Submit a static HTML form (including hidden CSRF tokens present in the initial HTML). JS-generated tokens and JS-only submit handlers are out of scope."
    )]
    async fn submit_form(
        &self,
        Parameters(params): Parameters<SubmitFormParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "submit_form",
            self.inner
                .submit_form(
                    &params.url,
                    params.form.as_deref(),
                    params.fields.unwrap_or(serde_json::json!({})),
                    Some(self.cookie_session(&ctx, params.session)),
                    self.ignore_robots(params.ignore_robots),
                )
                .await,
        )
    }

    #[tool(
        description = "Invoke a schema.org JSON-LD Action discovered on a page. Re-fetches the declaring page, resolves the named Action, then follows its HTTP EntryPoint using url/urlTemplate, httpMethod, and encodingType. Annotation-only Actions without a concrete EntryPoint are not executed."
    )]
    async fn call_jsonld_action(
        &self,
        Parameters(params): Parameters<JsonLdActionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> String {
        encode(
            "call_jsonld_action",
            self.inner
                .call_jsonld_action(
                    &params.url,
                    &params.name,
                    params.arguments.unwrap_or(serde_json::json!({})),
                    Some(self.cookie_session(&ctx, params.session)),
                    self.ignore_robots(params.ignore_robots),
                )
                .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentNavigatorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                rmcp::model::Implementation::new("agent-navigator", env!("CARGO_PKG_VERSION"))
                    .with_website_url("https://github.com/bolshchikov/agent-navigator"),
            )
            .with_instructions(
                "AgentNavigator is an agent-native HTTP client. It never executes JavaScript or renders a DOM. \
Use navigate first. If capability_tier is js-required, escalate to a browser-automation tool — do not retry with a spoofed User-Agent. \
WebMCP support is Declarative API only (form toolname/tooldescription attributes). \
robots.txt is respected unless ignore_robots is explicitly true. On the public HTTP demo, ignore_robots, include_html, and caller headers are disabled.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let tools = self.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        tracing::info!(
            count = tools.len(),
            tools = ?names,
            "MCP tools/list — AgentNavigator JSON-RPC tools advertised to the client (not page WebMCP tools)"
        );
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let args = request
            .arguments
            .as_ref()
            .map(|m| serde_json::Value::Object(m.clone()))
            .unwrap_or_else(|| serde_json::json!({}));
        tracing::info!(
            name = %request.name,
            arguments = %compact_json(&args),
            "MCP tools/call — client invoked an AgentNavigator tool over JSON-RPC"
        );
        let name = request.name.clone();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        match &result {
            Ok(_) => tracing::info!(name = %name, "MCP tools/call completed"),
            Err(err) => {
                tracing::warn!(name = %name, error = %err, "MCP tools/call failed at JSON-RPC layer")
            }
        }
        result
    }

    async fn on_initialized(&self, _context: rmcp::service::NotificationContext<rmcp::RoleServer>) {
        tracing::info!(
            "MCP client initialized. Declarative WebMCP on a page is HTML <form toolname tooldescription>; calling it uses call_webmcp_tool, which issues a normal HTTP request — the website is never spoken to with MCP JSON-RPC."
        );
    }
}

impl AgentNavigatorMcp {
    fn to_nav(&self, params: NavigateParams, ctx: &RequestContext<RoleServer>) -> NavigateRequest {
        NavigateRequest {
            url: params.url,
            session: Some(self.cookie_session(ctx, params.session)),
            include_html: if self.public {
                false
            } else {
                params.include_html.unwrap_or(false)
            },
            ignore_robots: self.ignore_robots(params.ignore_robots),
            force_tier: params.force_tier.as_deref().and_then(parse_tier),
            headers: if self.public {
                Default::default()
            } else {
                params.headers.unwrap_or_default()
            },
            ..Default::default()
        }
    }
}

fn encode(op: &str, env: FetchEnvelope) -> String {
    log_envelope(op, &env);
    serde_json::to_string(&env).unwrap_or_else(|e| {
        format!(r#"{{"status":{{"kind":"error","code":"serialize","message":"{e}"}}}}"#)
    })
}

fn log_envelope(op: &str, env: &FetchEnvelope) {
    let (status, detail) = match &env.status {
        EnvelopeStatus::Ok => ("ok", String::new()),
        EnvelopeStatus::Escalation { escalation, reason } => {
            ("escalation", format!("{escalation:?}: {reason}"))
        }
        EnvelopeStatus::Error { code, message } => ("error", format!("{code}: {message}")),
    };
    tracing::info!(
        op,
        status,
        detail = %detail,
        capability_tier = env.capability_tier.as_str(),
        http_status = env.metadata.http_status,
        final_url = %env.metadata.final_url,
        page_tools = env.tools_available.len(),
        "MCP tool result envelope"
    );
    if env.tools_available.is_empty() {
        tracing::info!(
            op,
            "no page-declared tools on this URL (no <form toolname+tooldescription>, JSON-LD Action, or HTML form)"
        );
        return;
    }
    for tool in &env.tools_available {
        let invocation = match &tool.invocation {
            ToolInvocation::Http { method, url, .. } => format!("{method} {url}"),
            ToolInvocation::Unavailable { reason } => format!("unavailable ({reason})"),
        };
        tracing::info!(
            op,
            name = %tool.name,
            kind = ?tool.kind,
            invocation = %invocation,
            "page-declared tool — invoke via call_webmcp_tool / submit_form / call_jsonld_action; this is HTTP, not MCP JSON-RPC to the site"
        );
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    const MAX: usize = 500;
    let s = value.to_string();
    if s.len() <= MAX {
        s
    } else {
        format!(
            "{}… ({} bytes)",
            s.chars().take(MAX).collect::<String>(),
            s.len()
        )
    }
}
