use std::collections::BTreeMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Consistent agent-facing response envelope (PRD FR28).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FetchEnvelope {
    pub status: EnvelopeStatus,
    pub capability_tier: CapabilityTier,
    pub content: Content,
    pub metadata: PageMetadata,
    pub tools_available: Vec<AvailableTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvelopeStatus {
    Ok,
    Escalation {
        escalation: EscalationKind,
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationKind {
    JsRequired,
    AuthRequiresBrowser,
    WebmcpImperativeOnly,
    RobotsDisallowed,
    SiteBlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityTier {
    Structured,
    StaticReadable,
    JsRequired,
}

impl CapabilityTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::StaticReadable => "static-readable",
            Self::JsRequired => "js-required",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Content {
    /// Readability-style markdown. Empty string when nothing extractable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Raw HTML only when the caller asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Structured payloads (JSON-LD actions, parsed llms.txt, JSON responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PageMetadata {
    pub requested_url: String,
    pub final_url: String,
    pub http_status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    pub redirect_chain: Vec<String>,
    pub capabilities: Vec<String>,
    pub classification_reason: String,
    pub classification_overridden: bool,
    pub webmcp: WebmcpBoundary,
    pub open_graph: BTreeMap<String, String>,
    pub json_ld: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llms_txt: Option<LlmsTxtDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llms_full_txt: Option<LlmsTxtDocument>,
    pub session: String,
    pub robots_checked: bool,
    pub robots_allowed: bool,
    pub ignore_robots: bool,
    pub user_agent: String,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebmcpBoundary {
    pub spec: String,
    pub declarative_supported: bool,
    pub imperative_supported: bool,
    pub imperative_detected: bool,
    pub note: String,
}

impl Default for WebmcpBoundary {
    fn default() -> Self {
        Self {
            spec: "webmcp-declarative-2026-05".to_string(),
            declarative_supported: true,
            imperative_supported: false,
            imperative_detected: false,
            note: "AgentNavigator supports the WebMCP Declarative API only. The Imperative API (navigator.modelContext.registerTool) requires JavaScript execution and is out of scope. Escalate to a browser-automation tool if the site only registers tools imperatively.".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmsTxtDocument {
    pub source_url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    pub sections: Vec<LlmsSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmsSection {
    pub name: String,
    pub optional: bool,
    pub links: Vec<LlmsLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmsLink {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AvailableTool {
    pub name: String,
    pub description: String,
    pub kind: ToolKind,
    pub input_schema: serde_json::Value,
    pub invocation: ToolInvocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown_attributes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// AgentNavigator always HTTP-submits declarative tools. Missing `toolautosubmit`
    /// is treated as present — there is no browser confirmation UI.
    #[serde(default = "default_true")]
    pub autosubmit: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    WebmcpDeclarative,
    HtmlForm,
    JsonLdAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolInvocation {
    Http {
        method: String,
        url: String,
        enctype: String,
        /// Hidden/default field values from the initial HTML (CSRF tokens, etc.).
        /// Overlay caller arguments on top when invoking.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        default_fields: std::collections::HashMap<String, String>,
    },
    Unavailable {
        reason: String,
    },
}

impl FetchEnvelope {
    pub fn error(
        requested_url: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        elapsed: Duration,
        user_agent: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        let requested_url = requested_url.into();
        Self {
            status: EnvelopeStatus::Error {
                code: code.into(),
                message: message.into(),
            },
            // Unclassified failure — not a JS-escalation signal.
            capability_tier: CapabilityTier::StaticReadable,
            content: Content::default(),
            metadata: PageMetadata {
                requested_url: requested_url.clone(),
                final_url: requested_url,
                http_status: 0,
                content_type: None,
                title: None,
                canonical_url: None,
                description: None,
                language: None,
                last_modified: None,
                redirect_chain: Vec::new(),
                capabilities: Vec::new(),
                classification_reason: "request failed before classification".into(),
                classification_overridden: false,
                webmcp: WebmcpBoundary::default(),
                open_graph: BTreeMap::new(),
                json_ld: Vec::new(),
                llms_txt: None,
                llms_full_txt: None,
                session: session.into(),
                robots_checked: false,
                robots_allowed: true,
                ignore_robots: false,
                user_agent: user_agent.into(),
                elapsed_ms: elapsed.as_millis() as u64,
                warnings: Vec::new(),
            },
            tools_available: Vec::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self.status, EnvelopeStatus::Ok)
    }
}
