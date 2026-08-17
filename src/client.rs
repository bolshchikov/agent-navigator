use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use reqwest::Method;
use scraper::Html;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::classify::{classify, ClassificationSignals};
use crate::config::ClientConfig;
use crate::discover::{discover_in_html, parse_llms_txt, HtmlForm};
use crate::envelope::{
    AvailableTool, CapabilityTier, Content, EnvelopeStatus, EscalationKind, FetchEnvelope,
    PageMetadata, ToolInvocation, ToolKind, WebmcpBoundary,
};
use crate::extract::ExtractedPage;
use crate::http::{
    ensure_fetchable_url, origin_of, send, CachedDiscovery, DiscoveryCache, FetchContext,
    HttpRequest, HttpResponse, RequestBody, RobotsCache,
};
use crate::session::SessionStore;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NavigateRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub include_html: bool,
    #[serde(default)]
    pub ignore_robots: bool,
    #[serde(default)]
    pub force_tier: Option<CapabilityTier>,
    #[serde(default)]
    pub enctype: Option<String>,
}

pub struct AgentNavigator {
    pub config: ClientConfig,
    sessions: SessionStore,
    robots: RobotsCache,
    discovery: DiscoveryCache,
    probe: reqwest::Client,
}

impl AgentNavigator {
    pub fn new(config: ClientConfig) -> crate::error::Result<Self> {
        let sessions = SessionStore::new(config.session_dir.clone())?;
        let probe = crate::http::probe_client(&config)?;
        Ok(Self {
            config,
            sessions,
            robots: RobotsCache::new(),
            discovery: DiscoveryCache::new(),
            probe,
        })
    }

    pub async fn navigate(&self, req: NavigateRequest) -> FetchEnvelope {
        let started = Instant::now();
        let session_name = req.session.clone().unwrap_or_else(|| "default".into());
        tracing::info!(
            url = %req.url,
            method = %req.method.as_deref().unwrap_or("GET"),
            session = %session_name,
            ignore_robots = req.ignore_robots,
            "navigate"
        );
        match self.navigate_inner(&req, &session_name, started).await {
            Ok(env) => env,
            Err(err) => {
                let mut env = FetchEnvelope::error(
                    req.url,
                    err.code(),
                    err.to_string(),
                    started.elapsed(),
                    self.config.user_agent.clone(),
                    session_name,
                );
                if let crate::error::Error::RobotsDisallowed { url } = &err {
                    env.status = EnvelopeStatus::Escalation {
                        escalation: EscalationKind::RobotsDisallowed,
                        reason: format!("robots.txt disallows {url}"),
                    };
                    env.metadata.robots_checked = true;
                    env.metadata.robots_allowed = false;
                }
                env
            }
        }
    }

    pub async fn extract(&self, req: NavigateRequest) -> FetchEnvelope {
        self.navigate(req).await
    }

    pub async fn discover(&self, req: NavigateRequest) -> FetchEnvelope {
        let mut env = self.navigate(req).await;
        env.content.markdown = None;
        env
    }

    pub async fn call_webmcp_tool(
        &self,
        page_url: &str,
        tool_name: &str,
        args: serde_json::Value,
        session: Option<String>,
        ignore_robots: bool,
    ) -> FetchEnvelope {
        tracing::info!(
            page_url,
            tool_name,
            arguments = %args,
            "call_webmcp_tool: fetch the page, look for <form toolname tooldescription>, then POST/GET the form action over HTTP"
        );
        let env = self
            .navigate(NavigateRequest {
                url: page_url.to_string(),
                session: session.clone(),
                ignore_robots,
                ..Default::default()
            })
            .await;
        if matches!(env.status, EnvelopeStatus::Error { .. })
            || matches!(
                env.status,
                EnvelopeStatus::Escalation {
                    escalation: EscalationKind::RobotsDisallowed | EscalationKind::SiteBlocked,
                    ..
                }
            )
        {
            tracing::warn!(
                page_url,
                tool_name,
                status = ?env.status,
                "call_webmcp_tool: page fetch did not succeed; not invoking a tool"
            );
            return env;
        }
        let declarative: Vec<&str> = env
            .tools_available
            .iter()
            .filter(|t| t.kind == ToolKind::WebmcpDeclarative)
            .map(|t| t.name.as_str())
            .collect();
        tracing::info!(
            page_url,
            wanted = tool_name,
            found = ?declarative,
            imperative_detected = env.metadata.webmcp.imperative_detected,
            "call_webmcp_tool: declarative tools parsed from HTML"
        );
        let Some(tool) = env
            .tools_available
            .iter()
            .find(|t| t.name == tool_name && t.kind == ToolKind::WebmcpDeclarative)
            .cloned()
        else {
            if env.metadata.webmcp.imperative_detected {
                tracing::warn!(
                    page_url,
                    tool_name,
                    "call_webmcp_tool: no declarative tool; site only registers tools via navigator.modelContext.registerTool (JS)"
                );
                return self.escalation_envelope(
                    page_url,
                    EscalationKind::WebmcpImperativeOnly,
                    format!("tool '{tool_name}' was not found as a declarative WebMCP tool. Imperative WebMCP was detected; AgentNavigator does not execute JavaScript."),
                    session,
                );
            }
            if !matches!(env.status, EnvelopeStatus::Ok) {
                return env;
            }
            tracing::warn!(
                page_url,
                tool_name,
                found = ?declarative,
                "call_webmcp_tool: no matching <form toolname>"
            );
            let mut failed = env;
            failed.status = EnvelopeStatus::Error {
                code: "tool_not_found".into(),
                message: format!("no declarative WebMCP tool named '{tool_name}' on {page_url}"),
            };
            return failed;
        };

        invoke_http_tool(self, tool, args, session, ignore_robots).await
    }

    pub async fn submit_form(
        &self,
        page_url: &str,
        form_name: Option<&str>,
        args: serde_json::Value,
        session: Option<String>,
        ignore_robots: bool,
    ) -> FetchEnvelope {
        let env = self
            .navigate(NavigateRequest {
                url: page_url.to_string(),
                session: session.clone(),
                ignore_robots,
                ..Default::default()
            })
            .await;
        if matches!(env.status, EnvelopeStatus::Error { .. })
            || matches!(
                env.status,
                EnvelopeStatus::Escalation {
                    escalation: EscalationKind::RobotsDisallowed | EscalationKind::SiteBlocked,
                    ..
                }
            )
        {
            return env;
        }
        let Some(tool) = pick_form(&env.tools_available, form_name) else {
            if !matches!(env.status, EnvelopeStatus::Ok) {
                return env;
            }
            let mut failed = env;
            failed.status = EnvelopeStatus::Error {
                code: "form_not_found".into(),
                message: format!("no HTML form matching {form_name:?} on {page_url}"),
            };
            return failed;
        };
        invoke_http_tool(self, tool, args, session, ignore_robots).await
    }

    pub async fn call_jsonld_action(
        &self,
        page_url: &str,
        action_name: &str,
        args: serde_json::Value,
        session: Option<String>,
        ignore_robots: bool,
    ) -> FetchEnvelope {
        let env = self
            .navigate(NavigateRequest {
                url: page_url.to_string(),
                session: session.clone(),
                ignore_robots,
                ..Default::default()
            })
            .await;
        if matches!(env.status, EnvelopeStatus::Error { .. })
            || matches!(
                env.status,
                EnvelopeStatus::Escalation {
                    escalation: EscalationKind::RobotsDisallowed | EscalationKind::SiteBlocked,
                    ..
                }
            )
        {
            return env;
        }
        let Some(tool) = env
            .tools_available
            .iter()
            .find(|tool| tool.kind == ToolKind::JsonLdAction && tool.name == action_name)
            .cloned()
        else {
            if !matches!(env.status, EnvelopeStatus::Ok) {
                return env;
            }
            let mut failed = env;
            failed.status = EnvelopeStatus::Error {
                code: "jsonld_action_not_found".into(),
                message: format!(
                    "no schema.org Action named '{action_name}' declared on {page_url}"
                ),
            };
            return failed;
        };
        if let ToolInvocation::Unavailable { reason } = &tool.invocation {
            let mut failed = env;
            failed.status = EnvelopeStatus::Error {
                code: "jsonld_action_not_invocable".into(),
                message: reason.clone(),
            };
            return failed;
        }
        if let Err(message) = validate_tool_arguments(&tool, &args) {
            let mut failed = env;
            failed.status = EnvelopeStatus::Error {
                code: "invalid_tool_arguments".into(),
                message,
            };
            return failed;
        }
        invoke_http_tool(self, tool, args, session, ignore_robots).await
    }

    async fn navigate_inner(
        &self,
        req: &NavigateRequest,
        session_name: &str,
        started: Instant,
    ) -> crate::error::Result<FetchEnvelope> {
        let url = Url::parse(&req.url)?;
        ensure_fetchable_url(&url)?;
        let ignore_robots = req.ignore_robots || self.config.ignore_robots;
        if ignore_robots {
            tracing::warn!(
                url = %url,
                "robots.txt opt-out is on; fetch will not consult robots.txt"
            );
        }
        let session = self.sessions.get_or_create(session_name, &self.config)?;

        let (robots_allowed, _robots_checked, _) = self
            .robots
            .allowed(&self.probe, &self.config, &url, ignore_robots)
            .await?;
        if !robots_allowed {
            return Ok(robots_denied_envelope(
                req,
                session_name,
                &self.config.user_agent,
                started,
            ));
        }

        let method = parse_method(req.method.as_deref().unwrap_or("GET"))?;
        let body = json_to_body(req.body.as_ref(), &method, req.enctype.as_deref());
        let response = match send(
            &session,
            &self.config,
            HttpRequest {
                method,
                url: url.clone(),
                headers: req.headers.clone(),
                body,
                timeout: None,
            },
            &FetchContext {
                robots: &self.robots,
                probe: &self.probe,
                ignore_robots,
            },
        )
        .await
        {
            Ok(resp) => resp,
            Err(crate::error::Error::RobotsDisallowed { url: denied }) => {
                return Ok(robots_denied_url(
                    req,
                    &denied,
                    session_name,
                    &self.config.user_agent,
                    started,
                ));
            }
            Err(err) => return Err(err),
        };
        let _ = self.sessions.persist(&session);

        let mut envelope = self
            .build_envelope(req, session_name, &url, &response, started)
            .await?;

        match response.status.as_u16() {
            401 => {
                envelope.status = EnvelopeStatus::Escalation {
                    escalation: EscalationKind::AuthRequiresBrowser,
                    reason: "HTTP 401 — authentication required. OAuth/MFA/JS login flows are out of scope; supply a bearer token or a static HTML login form.".into(),
                };
            }
            403 => {
                envelope.status = EnvelopeStatus::Escalation {
                    escalation: EscalationKind::SiteBlocked,
                    reason: "HTTP 403 — the site refused this non-browser client. AgentNavigator will not spoof a browser fingerprint.".into(),
                };
            }
            _ => {}
        }

        Ok(envelope)
    }

    async fn build_envelope(
        &self,
        req: &NavigateRequest,
        session_name: &str,
        requested: &Url,
        response: &HttpResponse,
        started: Instant,
    ) -> crate::error::Result<FetchEnvelope> {
        let mut warnings = Vec::new();
        warnings.extend(response.warnings.iter().cloned());
        let body_text = response.text();

        if response.is_json() {
            let structured = serde_json::from_str(&body_text).ok();
            return Ok(simple_envelope(
                req,
                session_name,
                requested,
                response,
                started,
                &self.config.user_agent,
                CapabilityTier::Structured,
                EnvelopeStatus::Ok,
                Content {
                    markdown: None,
                    html: if req.include_html {
                        Some(body_text)
                    } else {
                        None
                    },
                    structured,
                },
                vec!["json".into()],
                "response is JSON; treated as structured",
                warnings,
                Vec::new(),
            ));
        }

        if !response.is_html() {
            if crate::discover::looks_like_llms_url(response.final_url.as_str()) {
                if let Some(doc) = parse_llms_txt(response.final_url.as_str(), &body_text) {
                    let mut env = simple_envelope(
                        req,
                        session_name,
                        requested,
                        response,
                        started,
                        &self.config.user_agent,
                        CapabilityTier::Structured,
                        EnvelopeStatus::Ok,
                        Content {
                            markdown: Some(body_text.clone()),
                            html: None,
                            structured: serde_json::to_value(&doc).ok(),
                        },
                        vec!["llms_txt".into()],
                        "plain-text llms.txt document",
                        warnings,
                        Vec::new(),
                    );
                    env.metadata.llms_txt = Some(doc);
                    return Ok(env);
                }
            }
            return Ok(simple_envelope(
                req,
                session_name,
                requested,
                response,
                started,
                &self.config.user_agent,
                CapabilityTier::StaticReadable,
                EnvelopeStatus::Ok,
                Content {
                    markdown: Some(body_text),
                    html: None,
                    structured: None,
                },
                Vec::new(),
                "non-HTML response returned as text",
                warnings,
                Vec::new(),
            ));
        }

        let origin = origin_of(&response.final_url);
        let cached = self.discover_domain(&origin, &response.final_url).await;
        warnings.extend(cached.warnings.iter().cloned());

        let document = Html::parse_document(&body_text);
        let extracted =
            crate::extract::extract_from_document(&document, response.final_url.as_str());
        let page_disc = discover_in_html(&document, &response.final_url);

        let mut capabilities = Vec::new();
        if cached.llms_txt.is_some() {
            capabilities.push("llms_txt".into());
        }
        if cached.llms_full_txt.is_some() {
            capabilities.push("llms_full_txt".into());
        }
        if page_disc.forms.iter().any(HtmlForm::is_webmcp) {
            capabilities.push("webmcp_declarative".into());
        }
        if page_disc.imperative_webmcp {
            capabilities.push("webmcp_imperative_unsupported".into());
        }
        if !page_disc.json_ld_actions.is_empty() {
            capabilities.push("json_ld_actions".into());
        } else if !page_disc.json_ld.is_empty() {
            capabilities.push("json_ld".into());
        }
        if !page_disc.open_graph.is_empty() {
            capabilities.push("open_graph".into());
        }
        if page_disc.forms.iter().any(|f| !f.is_webmcp()) {
            capabilities.push("html_forms".into());
        }

        let signals = ClassificationSignals {
            has_webmcp_declarative: page_disc.forms.iter().any(HtmlForm::is_webmcp),
            has_llms_txt: cached.llms_txt.is_some() || cached.llms_full_txt.is_some(),
            has_json_ld_actions: !page_disc.json_ld_actions.is_empty(),
            imperative_webmcp_only: page_disc.imperative_webmcp
                && !page_disc.forms.iter().any(HtmlForm::is_webmcp),
        };

        let host = response.final_url.host_str().unwrap_or("");
        let override_tier = req.force_tier.or_else(|| self.config.override_for(host));
        let classification = classify(&extracted, &signals, override_tier);

        let webmcp = WebmcpBoundary {
            imperative_detected: page_disc.imperative_webmcp,
            ..WebmcpBoundary::default()
        };

        let mut structured_payload = serde_json::Map::new();
        if !page_disc.json_ld_actions.is_empty() {
            structured_payload.insert("json_ld_actions".into(), json!(page_disc.json_ld_actions));
        }
        if let Some(llms) = &cached.llms_txt {
            structured_payload.insert(
                "llms_txt".into(),
                serde_json::to_value(llms).unwrap_or(json!({})),
            );
        }

        let status = match classification.tier {
            CapabilityTier::JsRequired => EnvelopeStatus::Escalation {
                escalation: if signals.imperative_webmcp_only {
                    EscalationKind::WebmcpImperativeOnly
                } else {
                    EscalationKind::JsRequired
                },
                reason: classification.reason.clone(),
            },
            _ => EnvelopeStatus::Ok,
        };

        let markdown = if extracted.markdown.trim().is_empty() {
            None
        } else {
            Some(extracted.markdown.clone())
        };

        Ok(FetchEnvelope {
            status,
            capability_tier: classification.tier,
            content: Content {
                markdown,
                html: if req.include_html {
                    Some(body_text)
                } else {
                    None
                },
                structured: if structured_payload.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(structured_payload))
                },
            },
            metadata: html_metadata(
                req,
                session_name,
                requested,
                response,
                started,
                &self.config.user_agent,
                &extracted,
                capabilities,
                classification.reason,
                classification.overridden,
                warnings,
                webmcp,
                page_disc.open_graph,
                page_disc.json_ld,
                cached.llms_txt,
                cached.llms_full_txt,
            ),
            tools_available: page_disc.tools,
        })
    }

    async fn discover_domain(&self, origin: &str, page_url: &Url) -> CachedDiscovery {
        if let Some(hit) = self.discovery.get(origin) {
            return hit;
        }
        let mut warnings = Vec::new();
        let mut candidates = llms_candidate_urls(page_url);
        let origin_llms = format!("{origin}/llms.txt");
        if !candidates.contains(&origin_llms) {
            candidates.push(origin_llms);
        }
        let origin_full = format!("{origin}/llms-full.txt");

        let mut llms_txt = None;
        for url in &candidates {
            match fetch_llms(&self.probe, url).await {
                Ok(Some(doc)) => {
                    llms_txt = Some(doc);
                    break;
                }
                Ok(None) => {}
                Err(err) => warnings.push(format!("llms.txt probe {url}: {err}")),
            }
        }
        let llms_full_txt = match fetch_llms(&self.probe, &origin_full).await {
            Ok(doc) => doc,
            Err(err) => {
                warnings.push(format!("llms-full.txt probe: {err}"));
                None
            }
        };

        let entry = CachedDiscovery::fresh(llms_txt, llms_full_txt, warnings);
        self.discovery
            .insert(origin.to_string(), entry.clone(), self.config.discovery_ttl);
        entry
    }

    fn escalation_envelope(
        &self,
        url: &str,
        kind: EscalationKind,
        reason: String,
        session: Option<String>,
    ) -> FetchEnvelope {
        let mut env = FetchEnvelope::error(
            url,
            "escalation",
            &reason,
            std::time::Duration::ZERO,
            self.config.user_agent.clone(),
            session.unwrap_or_else(|| "default".into()),
        );
        env.status = EnvelopeStatus::Escalation {
            escalation: kind,
            reason,
        };
        env.capability_tier = match kind {
            EscalationKind::JsRequired | EscalationKind::WebmcpImperativeOnly => {
                CapabilityTier::JsRequired
            }
            _ => env.capability_tier,
        };
        env
    }
}

async fn invoke_http_tool(
    client: &AgentNavigator,
    tool: AvailableTool,
    args: serde_json::Value,
    session: Option<String>,
    ignore_robots: bool,
) -> FetchEnvelope {
    match tool.invocation {
        ToolInvocation::Http {
            method,
            url,
            enctype,
            default_fields,
        } => {
            let mut body_fields = serde_json::Map::new();
            for (key, value) in default_fields {
                body_fields.insert(key, serde_json::Value::String(value));
            }
            if let Some(arguments) = args.as_object() {
                body_fields.extend(arguments.clone());
            }
            let mut fields = json_to_form_map(&serde_json::Value::Object(body_fields.clone()));
            let template_keys: HashSet<_> = fields.keys().cloned().collect();
            let mut url = apply_url_template(&url, &mut fields);
            let remaining_keys: HashSet<_> = fields.keys().cloned().collect();
            for consumed in template_keys.difference(&remaining_keys) {
                body_fields.remove(consumed);
            }
            tracing::info!(
                tool = %tool.name,
                method = %method,
                url = %url,
                "invoking page-declared HTTP tool"
            );
            let body = if method.eq_ignore_ascii_case("GET") {
                url = append_query(&url, &fields);
                None
            } else {
                Some(serde_json::Value::Object(body_fields))
            };
            let mut warnings_note = Vec::new();
            let send_enctype = if enctype.eq_ignore_ascii_case("multipart/form-data") {
                warnings_note.push(
                    "multipart/form-data file uploads are not supported; fields are sent as application/x-www-form-urlencoded".to_string(),
                );
                Some("application/x-www-form-urlencoded".into())
            } else if enctype.is_empty() {
                None
            } else {
                Some(enctype)
            };
            let mut env = client
                .navigate(NavigateRequest {
                    url,
                    method: Some(method),
                    session,
                    body,
                    ignore_robots,
                    enctype: send_enctype,
                    ..Default::default()
                })
                .await;
            env.metadata.warnings.extend(warnings_note);
            env
        }
        ToolInvocation::Unavailable { reason } => {
            client.escalation_envelope("", EscalationKind::JsRequired, reason, session)
        }
    }
}

fn pick_form(tools: &[AvailableTool], name: Option<&str>) -> Option<AvailableTool> {
    let forms: Vec<_> = tools
        .iter()
        .filter(|t| matches!(t.kind, ToolKind::HtmlForm | ToolKind::WebmcpDeclarative))
        .cloned()
        .collect();
    match name {
        Some(n) => forms.into_iter().find(|t| t.name == n),
        None => forms.into_iter().next(),
    }
}

fn parse_method(s: &str) -> crate::error::Result<Method> {
    match s.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        other => Err(crate::error::Error::Other(format!(
            "unsupported HTTP method {other}; allowed: GET, POST, PUT, PATCH, DELETE, HEAD"
        ))),
    }
}

fn json_to_body(
    body: Option<&serde_json::Value>,
    method: &Method,
    enctype: Option<&str>,
) -> Option<RequestBody> {
    if matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let value = body?;
    let enc = enctype.unwrap_or("").to_ascii_lowercase();
    if enc.starts_with("application/json") {
        return Some(RequestBody::Bytes {
            content_type: "application/json".into(),
            data: serde_json::to_vec(value).unwrap_or_default(),
        });
    }
    if let Some(map) = value.as_object() {
        if map
            .values()
            .all(|v| v.is_string() || v.is_number() || v.is_boolean() || v.is_null())
        {
            let form = json_to_form_map(value);
            return Some(RequestBody::Form(form));
        }
    }
    Some(RequestBody::Bytes {
        content_type: "application/json".into(),
        data: serde_json::to_vec(value).unwrap_or_default(),
    })
}

fn json_to_form_map(value: &serde_json::Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(map) = value.as_object() {
        for (k, v) in map {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                other => other.to_string().trim_matches('"').to_string(),
            };
            out.insert(k.clone(), s);
        }
    }
    out
}

fn validate_tool_arguments(tool: &AvailableTool, args: &serde_json::Value) -> Result<(), String> {
    let Some(arguments) = args.as_object() else {
        return Err(format!(
            "arguments for JSON-LD Action '{}' must be a JSON object",
            tool.name
        ));
    };
    let required = tool
        .input_schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str);
    let missing: Vec<_> = required
        .filter(|name| arguments.get(*name).is_none_or(serde_json::Value::is_null))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing required arguments for JSON-LD Action '{}': {}",
            tool.name,
            missing.join(", ")
        ))
    }
}

fn append_query(url: &str, fields: &HashMap<String, String>) -> String {
    if fields.is_empty() {
        return url.to_string();
    }
    let mut parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => return url.to_string(),
    };
    {
        let mut pairs = parsed.query_pairs_mut();
        for (k, v) in fields {
            pairs.append_pair(k, v);
        }
    }
    parsed.to_string()
}

/// Fill `{placeholder}` tokens from `fields`, removing consumed keys so GET does
/// not also append them as query params (and POST does not also send them in the body).
fn apply_url_template(template: &str, fields: &mut HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut remaining = template;
    let mut consumed = HashSet::new();

    while let Some(start) = remaining.find('{') {
        output.push_str(&remaining[..start]);
        let Some(end) = remaining[start + 1..].find('}') else {
            output.push_str(&remaining[start..]);
            remaining = "";
            break;
        };
        let whole = &remaining[start..start + end + 2];
        let expression = remaining[start + 1..start + end + 1].trim();
        let (operator, variables) = match expression.chars().next() {
            Some(operator @ ('?' | '&')) => (Some(operator), &expression[1..]),
            Some('+') => {
                output.push_str(whole);
                remaining = &remaining[start + end + 2..];
                continue;
            }
            _ => (None, expression),
        };
        let values: Vec<_> = variables
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .filter_map(|name| {
                lookup_template_value(name, fields)
                    .map(|(key, value)| (name.to_string(), key, value))
            })
            .collect();
        let variable_count = variables
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .count();

        if values.len() != variable_count {
            output.push_str(whole);
        } else if let Some(operator) = operator {
            if !values.is_empty() {
                output.push(operator);
                output.push_str(
                    &values
                        .iter()
                        .map(|(name, _, value)| {
                            format!(
                                "{}={}",
                                percent_encode_component(name),
                                percent_encode_component(value)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("&"),
                );
            }
        } else {
            output.push_str(
                &values
                    .iter()
                    .map(|(_, _, value)| percent_encode_component(value))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        consumed.extend(values.into_iter().map(|(_, key, _)| key));
        remaining = &remaining[start + end + 2..];
    }
    output.push_str(remaining);
    for key in consumed {
        fields.remove(&key);
    }
    output
}

fn lookup_template_value(name: &str, fields: &HashMap<String, String>) -> Option<(String, String)> {
    if let Some(v) = fields.get(name) {
        return Some((name.to_string(), v.clone()));
    }
    const SEARCH_ALIASES: &[&str] = &["search_term_string", "search_term", "query", "q"];
    if SEARCH_ALIASES.contains(&name) {
        for alias in SEARCH_ALIASES {
            if let Some(v) = fields.get(*alias) {
                return Some(((*alias).to_string(), v.clone()));
            }
        }
    }
    None
}

fn percent_encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn llms_candidate_urls(page_url: &Url) -> Vec<String> {
    let mut urls = Vec::new();
    let origin = origin_of(page_url);
    let path = page_url.path();
    let mut prefix = path.trim_end_matches('/');
    if prefix.is_empty() {
        urls.push(format!("{origin}/llms.txt"));
        return urls;
    }
    // Walk up from the current directory.
    while let Some(idx) = prefix.rfind('/') {
        prefix = &prefix[..idx];
        let dir = if prefix.is_empty() { "" } else { prefix };
        urls.push(format!("{origin}{dir}/llms.txt"));
        if prefix.is_empty() {
            break;
        }
    }
    urls
}

async fn fetch_llms(
    client: &reqwest::Client,
    url: &str,
) -> crate::error::Result<Option<crate::envelope::LlmsTxtDocument>> {
    let parsed = Url::parse(url)?;
    if ensure_fetchable_url(&parsed).is_err() {
        return Ok(None);
    }
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let text = resp.text().await.unwrap_or_default();
    Ok(parse_llms_txt(url, &text))
}

fn robots_denied_envelope(
    req: &NavigateRequest,
    session: &str,
    user_agent: &str,
    started: Instant,
) -> FetchEnvelope {
    robots_denied_url(req, &req.url, session, user_agent, started)
}

fn robots_denied_url(
    req: &NavigateRequest,
    denied_url: &str,
    session: &str,
    user_agent: &str,
    started: Instant,
) -> FetchEnvelope {
    let mut env = FetchEnvelope::error(
        req.url.clone(),
        "robots_disallowed",
        format!(
            "robots.txt disallows AgentNavigator from fetching {denied_url}. Pass --ignore-robots only when you have other authorization. This opt-out is explicit and logged."
        ),
        started.elapsed(),
        user_agent,
        session,
    );
    env.status = EnvelopeStatus::Escalation {
        escalation: EscalationKind::RobotsDisallowed,
        reason: format!("robots.txt disallows {denied_url}"),
    };
    env.metadata.final_url = denied_url.to_string();
    env.metadata.robots_checked = true;
    env.metadata.robots_allowed = false;
    env.metadata.ignore_robots = false;
    env
}

fn simple_envelope(
    req: &NavigateRequest,
    session: &str,
    requested: &Url,
    response: &HttpResponse,
    started: Instant,
    user_agent: &str,
    tier: CapabilityTier,
    status: EnvelopeStatus,
    content: Content,
    capabilities: Vec<String>,
    reason: &str,
    warnings: Vec<String>,
    tools: Vec<AvailableTool>,
) -> FetchEnvelope {
    FetchEnvelope {
        status,
        capability_tier: tier,
        content,
        metadata: PageMetadata {
            requested_url: requested.to_string(),
            final_url: response.final_url.to_string(),
            http_status: response.status.as_u16(),
            content_type: response.content_type(),
            title: None,
            canonical_url: None,
            description: None,
            language: None,
            last_modified: response.last_modified(),
            redirect_chain: response.redirect_chain.clone(),
            capabilities,
            classification_reason: reason.to_string(),
            classification_overridden: req.force_tier.is_some(),
            webmcp: WebmcpBoundary::default(),
            open_graph: BTreeMap::new(),
            json_ld: Vec::new(),
            llms_txt: None,
            llms_full_txt: None,
            session: session.to_string(),
            robots_checked: !req.ignore_robots,
            robots_allowed: true,
            ignore_robots: req.ignore_robots,
            user_agent: user_agent.to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            warnings,
        },
        tools_available: tools,
    }
}

#[allow(clippy::too_many_arguments)]
fn html_metadata(
    req: &NavigateRequest,
    session: &str,
    requested: &Url,
    response: &HttpResponse,
    started: Instant,
    user_agent: &str,
    extracted: &ExtractedPage,
    capabilities: Vec<String>,
    reason: String,
    overridden: bool,
    warnings: Vec<String>,
    webmcp: WebmcpBoundary,
    open_graph: BTreeMap<String, String>,
    json_ld: Vec<serde_json::Value>,
    llms_txt: Option<crate::envelope::LlmsTxtDocument>,
    llms_full_txt: Option<crate::envelope::LlmsTxtDocument>,
) -> PageMetadata {
    PageMetadata {
        requested_url: requested.to_string(),
        final_url: response.final_url.to_string(),
        http_status: response.status.as_u16(),
        content_type: response.content_type(),
        title: extracted.title.clone(),
        canonical_url: extracted.canonical_url.clone(),
        description: extracted.description.clone(),
        language: extracted.language.clone(),
        last_modified: extracted
            .last_modified
            .clone()
            .or_else(|| response.last_modified()),
        redirect_chain: response.redirect_chain.clone(),
        capabilities,
        classification_reason: reason,
        classification_overridden: overridden,
        webmcp,
        open_graph,
        json_ld,
        llms_txt,
        llms_full_txt,
        session: session.to_string(),
        robots_checked: !req.ignore_robots,
        robots_allowed: true,
        ignore_robots: req.ignore_robots,
        user_agent: user_agent.to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_url_template_fills_and_consumes_placeholder() {
        let mut fields = HashMap::from([("search_term_string".into(), "rust lang".into())]);
        let url = apply_url_template(
            "https://example.com/search?q={search_term_string}",
            &mut fields,
        );
        assert_eq!(url, "https://example.com/search?q=rust%20lang");
        assert!(fields.is_empty());
    }

    #[test]
    fn apply_url_template_fills_search_aliases() {
        let mut fields = HashMap::from([("q".into(), "rust".into())]);
        let url = apply_url_template(
            "https://example.com/search?q={search_term_string}",
            &mut fields,
        );
        assert_eq!(url, "https://example.com/search?q=rust");
        assert!(fields.is_empty());
    }

    #[test]
    fn apply_url_template_leaves_plain_urls_alone() {
        let mut fields = HashMap::from([("q".into(), "rust".into())]);
        let url = apply_url_template("https://example.com/search", &mut fields);
        assert_eq!(url, "https://example.com/search");
        assert_eq!(fields.get("q").map(String::as_str), Some("rust"));
    }

    #[test]
    fn apply_url_template_expands_query_operators_and_multiple_values() {
        let mut fields = HashMap::from([
            ("q".into(), "rust lang".into()),
            ("locale".into(), "en-US".into()),
        ]);
        let url = apply_url_template("https://example.com/search{?q,locale}", &mut fields);
        assert_eq!(url, "https://example.com/search?q=rust%20lang&locale=en-US");
        assert!(fields.is_empty());

        let mut fields = HashMap::from([("page".into(), "2".into())]);
        let url = apply_url_template("https://example.com/search?q=rust{&page}", &mut fields);
        assert_eq!(url, "https://example.com/search?q=rust&page=2");
        assert!(fields.is_empty());
    }

    #[test]
    fn apply_url_template_path_values_use_percent_encoding() {
        let mut fields = HashMap::from([("reservation_id".into(), "table 4".into())]);
        let url = apply_url_template(
            "https://example.com/reservations/{reservation_id}",
            &mut fields,
        );
        assert_eq!(url, "https://example.com/reservations/table%204");
        assert!(fields.is_empty());
    }
}
