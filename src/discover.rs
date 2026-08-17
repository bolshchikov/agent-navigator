use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};
use url::Url;

use crate::envelope::{
    AvailableTool, LlmsLink, LlmsSection, LlmsTxtDocument, ToolInvocation, ToolKind,
};

static JSON_LD: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("script[type='application/ld+json']").expect("json-ld"));
static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").expect("meta"));
static FORM: LazyLock<Selector> = LazyLock::new(|| Selector::parse("form").expect("form"));
static SCRIPT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("script").expect("script"));
static LABEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("label").expect("label"));

const KNOWN_FORM_TOOL_ATTRS: &[&str] = &["toolname", "tooldescription", "toolautosubmit"];

/// Spec snapshot this parser understands. Unknown `tool*` attributes are preserved, not dropped.
pub const WEBMCP_DECLARATIVE_SPEC: &str = "webmcp-declarative-2026-05";

#[derive(Debug, Clone, Default)]
pub struct PageDiscovery {
    pub open_graph: BTreeMap<String, String>,
    pub json_ld: Vec<Value>,
    pub json_ld_actions: Vec<Value>,
    pub tools: Vec<AvailableTool>,
    pub imperative_webmcp: bool,
    pub forms: Vec<HtmlForm>,
}

#[derive(Debug, Clone)]
pub struct HtmlForm {
    pub id: Option<String>,
    pub name: Option<String>,
    pub action: String,
    pub action_was_empty: bool,
    pub method: String,
    pub enctype: String,
    pub fields: Vec<FormField>,
    pub webmcp_name: Option<String>,
    pub webmcp_description: Option<String>,
    pub webmcp_autosubmit: bool,
    pub unknown_tool_attrs: Vec<String>,
}

impl HtmlForm {
    pub fn is_webmcp(&self) -> bool {
        self.webmcp_name.is_some() && self.webmcp_description.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub name: String,
    pub field_type: String,
    pub required: bool,
    pub description: Option<String>,
    pub value: Option<String>,
    pub options: Vec<(String, String)>,
    pub hidden: bool,
}

pub fn discover_in_html(document: &Html, page_url: &Url) -> PageDiscovery {
    let mut discovery = PageDiscovery {
        open_graph: extract_open_graph(document),
        json_ld: extract_json_ld(document),
        ..Default::default()
    };
    discovery.json_ld_actions = collect_actions(&discovery.json_ld);
    discovery.imperative_webmcp = detect_imperative_webmcp(document);
    discovery.forms = parse_forms(document, page_url);
    discovery.tools = synthesize_tools(&discovery, page_url);
    discovery
}

fn extract_open_graph(document: &Html) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for meta in document.select(&META) {
        let property = meta
            .value()
            .attr("property")
            .or_else(|| meta.value().attr("name"))
            .unwrap_or("");
        if let Some(key) = property
            .strip_prefix("og:")
            .or_else(|| property.strip_prefix("twitter:"))
        {
            if let Some(content) = meta.value().attr("content") {
                map.entry(key.to_string())
                    .or_insert_with(|| content.to_string());
            }
        }
    }
    map
}

fn extract_json_ld(document: &Html) -> Vec<Value> {
    let mut out = Vec::new();
    for script in document.select(&JSON_LD) {
        let raw = script.text().collect::<String>();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => out.extend(items),
            Ok(v) => out.push(v),
            Err(_) => {
                if let Ok(Value::Array(items)) =
                    serde_json::from_str::<Value>(&format!("[{trimmed}]"))
                {
                    out.extend(items);
                }
            }
        }
    }
    out
}

fn collect_actions(blocks: &[Value]) -> Vec<Value> {
    let mut actions = Vec::new();
    for block in blocks {
        walk_json_ld(block, &mut |value| {
            if action_type(value).is_some() {
                actions.push(value.clone());
            }
        });
    }
    actions
}

fn walk_json_ld(value: &Value, visit: &mut impl FnMut(&Value)) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_json_ld(item, visit);
            }
        }
        Value::Object(map) => {
            visit(value);
            for child in map.values() {
                walk_json_ld(child, visit);
            }
        }
        _ => {}
    }
}

fn json_ld_types(value: &Value) -> Option<Vec<String>> {
    let t = value.get("@type")?;
    match t {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        ),
        _ => None,
    }
}

fn detect_imperative_webmcp(document: &Html) -> bool {
    for script in document.select(&SCRIPT) {
        let src = script
            .value()
            .attr("src")
            .unwrap_or("")
            .to_ascii_lowercase();
        if src.contains("modelcontext") {
            return true;
        }
        let text = script.text().collect::<String>();
        if text.contains("modelContext.registerTool")
            || text.contains("navigator.modelContext")
            || text.contains("document.modelContext")
        {
            return true;
        }
    }
    false
}

fn parse_forms(document: &Html, page_url: &Url) -> Vec<HtmlForm> {
    let mut forms = Vec::new();
    for form in document.select(&FORM) {
        let action_raw = form.value().attr("action").unwrap_or("");
        let action_was_empty = action_raw.is_empty();
        let action = resolve_action(page_url, action_raw);
        let method = form
            .value()
            .attr("method")
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let enctype = form
            .value()
            .attr("enctype")
            .unwrap_or("application/x-www-form-urlencoded")
            .to_string();
        let unknown_tool_attrs = form
            .value()
            .attrs()
            .filter(|(name, _)| {
                let n = name.to_ascii_lowercase();
                n.starts_with("tool") && !KNOWN_FORM_TOOL_ATTRS.contains(&n.as_str())
            })
            .map(|(name, _)| name.to_string())
            .collect();
        forms.push(HtmlForm {
            id: form.value().attr("id").map(|s| s.to_string()),
            name: form.value().attr("name").map(|s| s.to_string()),
            action,
            action_was_empty,
            method,
            enctype,
            fields: parse_fields(document, form),
            webmcp_name: form.value().attr("toolname").map(|s| s.to_string()),
            webmcp_description: form.value().attr("tooldescription").map(|s| s.to_string()),
            webmcp_autosubmit: form.value().attr("toolautosubmit").is_some(),
            unknown_tool_attrs,
        });
    }
    forms
}

fn resolve_action(page_url: &Url, action: &str) -> String {
    if action.is_empty() {
        return page_url.to_string();
    }
    page_url
        .join(action)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| action.to_string())
}

fn parse_fields(document: &Html, form: ElementRef<'_>) -> Vec<FormField> {
    let Ok(controls) = Selector::parse("input, select, textarea") else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for el in form.select(&controls) {
        let name = match el.value().attr("name") {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let type_attr = el
            .value()
            .attr("type")
            .unwrap_or(match el.value().name() {
                "textarea" => "textarea",
                "select" => "select",
                _ => "text",
            })
            .to_ascii_lowercase();
        if matches!(type_attr.as_str(), "submit" | "button" | "image" | "reset") {
            continue;
        }
        let required = el.value().attr("required").is_some();
        let hidden = type_attr == "hidden";
        let description = param_description(document, el);
        let value = el.value().attr("value").map(|s| s.to_string()).or_else(|| {
            if el.value().name() == "textarea" {
                Some(collapse_ws(&el.text().collect::<String>()))
            } else {
                None
            }
        });
        let options = if el.value().name() == "select" {
            select_options(el)
        } else {
            Vec::new()
        };
        fields.push(FormField {
            name,
            field_type: type_attr,
            required,
            description,
            value,
            options,
            hidden,
        });
    }
    fields
}

fn param_description(document: &Html, el: ElementRef<'_>) -> Option<String> {
    if let Some(desc) = el.value().attr("toolparamdescription") {
        return Some(desc.to_string());
    }
    if let Some(desc) = el.value().attr("aria-description") {
        return Some(desc.to_string());
    }
    if let Some(id) = el.value().attr("id") {
        for label in document.select(&LABEL) {
            if label.value().attr("for") == Some(id) {
                let text = collapse_ws(&label.text().collect::<String>());
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn select_options(el: ElementRef<'_>) -> Vec<(String, String)> {
    let Ok(sel) = Selector::parse("option") else {
        return Vec::new();
    };
    el.select(&sel)
        .filter_map(|opt| {
            let title = collapse_ws(&opt.text().collect::<String>());
            let value = opt
                .value()
                .attr("value")
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| title.clone());
            if value.is_empty() {
                None
            } else {
                Some((value, title))
            }
        })
        .collect()
}

fn synthesize_tools(discovery: &PageDiscovery, page_url: &Url) -> Vec<AvailableTool> {
    let mut tools: Vec<AvailableTool> = discovery
        .forms
        .iter()
        .filter(|f| f.is_webmcp())
        .map(webmcp_tool_from_form)
        .collect();
    tools.extend(json_ld_action_tools(&discovery.json_ld_actions, page_url));
    tools.extend(
        discovery
            .forms
            .iter()
            .filter(|f| !f.is_webmcp())
            .filter(|f| f.fields.iter().any(|field| !field.hidden))
            .map(html_form_tool),
    );
    let declarative: Vec<&str> = tools
        .iter()
        .filter(|t| t.kind == ToolKind::WebmcpDeclarative)
        .map(|t| t.name.as_str())
        .collect();
    if !declarative.is_empty() {
        tracing::info!(
            tools = ?declarative,
            spec = WEBMCP_DECLARATIVE_SPEC,
            "parsed declarative WebMCP from HTML <form toolname tooldescription>; calling one is an HTTP form submit"
        );
    } else if discovery.imperative_webmcp {
        tracing::info!(
            "page uses imperative WebMCP (navigator.modelContext.registerTool); AgentNavigator cannot call those tools without JS"
        );
    } else {
        tracing::debug!("no declarative WebMCP forms on this page");
    }
    tools
}

fn webmcp_tool_from_form(form: &HtmlForm) -> AvailableTool {
    let name = form
        .webmcp_name
        .clone()
        .unwrap_or_else(|| "unnamed_tool".into());
    let description = form
        .webmcp_description
        .clone()
        .unwrap_or_else(|| "Declarative WebMCP tool".into());
    let mut notes = Vec::new();
    if form.action_was_empty {
        notes.push("form has no action attribute; submitting to the current page URL per HTML. If the site handles submit only in JavaScript, this HTTP call will not perform the intended action.".to_string());
    }
    if !form.webmcp_autosubmit {
        tracing::debug!(
            tool = %name,
            "toolautosubmit missing on form; defaulting to HTTP autosubmit"
        );
    }
    AvailableTool {
        name,
        description,
        kind: ToolKind::WebmcpDeclarative,
        input_schema: fields_to_schema(&form.fields, true),
        invocation: ToolInvocation::Http {
            method: form.method.clone(),
            url: form.action.clone(),
            enctype: form.enctype.clone(),
            default_fields: form_default_fields(form),
        },
        spec_version: Some(WEBMCP_DECLARATIVE_SPEC.to_string()),
        unknown_attributes: form.unknown_tool_attrs.clone(),
        notes,
        autosubmit: true,
    }
}

fn html_form_tool(form: &HtmlForm) -> AvailableTool {
    let name = form
        .id
        .clone()
        .or_else(|| form.name.clone())
        .unwrap_or_else(|| "form".to_string());
    AvailableTool {
        name,
        description: format!("HTML form submitting via {} to {}", form.method, form.action),
        kind: ToolKind::HtmlForm,
        input_schema: fields_to_schema(&form.fields, false),
        invocation: ToolInvocation::Http {
            method: form.method.clone(),
            url: form.action.clone(),
            enctype: form.enctype.clone(),
            default_fields: form_default_fields(form),
        },
        spec_version: None,
        unknown_attributes: Vec::new(),
        notes: vec!["Not a WebMCP tool — fallback HTML form. Hidden fields present in the initial HTML (including static CSRF tokens) are submitted automatically unless overridden.".to_string()],
        autosubmit: true,
    }
}

struct JsonLdHttpTarget {
    url: String,
    method: String,
    enctype: String,
}

fn json_ld_action_tools(actions: &[Value], page_url: &Url) -> Vec<AvailableTool> {
    let mut tools = Vec::new();
    let mut name_counts = HashMap::new();
    let mut used_names = HashSet::new();
    for action in actions {
        let Some(action_type) = action_type(action) else {
            continue;
        };
        let base_name = action
            .get("name")
            .and_then(Value::as_str)
            .map(tool_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| tool_name(&action_type));
        let count = name_counts.entry(base_name.clone()).or_insert(0usize);
        let name = loop {
            *count += 1;
            let candidate = if *count == 1 {
                base_name.clone()
            } else {
                format!("{base_name}_{count}")
            };
            if used_names.insert(candidate.clone()) {
                break candidate;
            }
        };
        let description = action
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Site-declared schema.org {action_type}"));
        let target = action
            .get("target")
            .and_then(|target| parse_http_target(target, page_url, action_type == "SearchAction"));
        let input_schema = action_input_schema(action, target.as_ref().map(|t| t.url.as_str()));
        let invocation = match target {
            Some(target) => ToolInvocation::Http {
                method: target.method,
                url: target.url,
                enctype: target.enctype,
                default_fields: HashMap::new(),
            },
            None => ToolInvocation::Unavailable {
                reason: format!(
                    "{action_type} does not declare a usable HTTP EntryPoint with url or urlTemplate"
                ),
            },
        };
        let mut notes = Vec::new();
        if matches!(&invocation, ToolInvocation::Unavailable { .. }) {
            notes.push(
                "This JSON-LD Action is descriptive metadata, not an invocable HTTP operation."
                    .to_string(),
            );
        }
        tools.push(AvailableTool {
            name,
            description,
            kind: ToolKind::JsonLdAction,
            input_schema,
            invocation,
            spec_version: Some(format!("schema.org/{action_type}")),
            unknown_attributes: Vec::new(),
            notes,
            autosubmit: true,
        });
    }
    tools
}

/// schema.org `EntryPoint`: `url` / `urlTemplate`, `httpMethod`, `encodingType`.
fn parse_http_target(
    target: &Value,
    page_url: &Url,
    allow_string_target: bool,
) -> Option<JsonLdHttpTarget> {
    match target {
        Value::String(url) if allow_string_target && supported_url_template(url) => {
            Some(JsonLdHttpTarget {
                url: resolve_http_url(page_url, url)?,
                method: "GET".into(),
                enctype: "application/x-www-form-urlencoded".into(),
            })
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| parse_http_target(item, page_url, allow_string_target)),
        Value::Object(map) => {
            let raw_url = map
                .get("urlTemplate")
                .or_else(|| map.get("url"))
                .and_then(|v| v.as_str())
                .filter(|url| !url.trim().is_empty())?;
            if !supported_url_template(raw_url) {
                return None;
            }
            let url = resolve_http_url(page_url, raw_url)?;
            let method = parse_http_method(map.get("httpMethod")).unwrap_or_else(|| "GET".into());
            let enctype = map
                .get("encodingType")
                .or_else(|| map.get("contentType"))
                .and_then(|v| v.as_str())
                .unwrap_or("application/x-www-form-urlencoded")
                .to_string();
            Some(JsonLdHttpTarget {
                url,
                method,
                enctype,
            })
        }
        _ => None,
    }
}

fn resolve_http_url(page_url: &Url, raw_url: &str) -> Option<String> {
    let resolved = page_url.join(raw_url).ok()?;
    if !matches!(resolved.scheme(), "http" | "https") || resolved.host_str().is_none() {
        return None;
    }

    if looks_like_http_url(raw_url) {
        Some(raw_url.to_string())
    } else {
        Some(resolved.to_string())
    }
}

fn looks_like_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("https://") || s.starts_with("http://")
}

fn parse_http_method(value: Option<&Value>) -> Option<String> {
    let raw = match value? {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().find_map(|v| v.as_str().map(str::to_string))?,
        _ => return None,
    };
    let method = raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|m| !m.is_empty())?
        .to_ascii_uppercase();
    matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE").then_some(method)
}

fn action_type(action: &Value) -> Option<String> {
    json_ld_types(action)?.into_iter().find_map(|value| {
        let name = value.rsplit(['/', '#', ':']).next().unwrap_or(&value);
        (name.ends_with("Action") && name != "Action").then(|| name.to_string())
    })
}

fn tool_name(value: &str) -> String {
    let mut output = String::new();
    let mut previous_was_separator = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && !previous_was_separator && !output.ends_with('_') {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !output.is_empty() && !output.ends_with('_') {
            output.push('_');
            previous_was_separator = true;
        }
    }
    output.trim_matches('_').to_string()
}

fn action_input_schema(action: &Value, target_url: Option<&str>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    if let Some(map) = action.as_object() {
        for (key, input) in map {
            let Some(default_name) = input_property_name(key) else {
                continue;
            };
            let (name, is_required) = parse_input_spec(input, &default_name);
            properties
                .entry(name.clone())
                .or_insert_with(|| json!({ "type": "string" }));
            if is_required && !required.contains(&name) {
                required.push(name);
            }
        }
    }

    if let Some(url) = target_url {
        for name in placeholder_names(url) {
            properties
                .entry(name.clone())
                .or_insert_with(|| json!({ "type": "string" }));
            if !required.contains(&name) {
                required.push(name);
            }
        }
    }

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required
    })
}

fn input_property_name(key: &str) -> Option<String> {
    key.strip_suffix("-input")
        .or_else(|| key.strip_suffix("Input"))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Supports `"required name=search_term_string"` and PropertyValueSpecification.
fn parse_input_spec(input: &Value, default_name: &str) -> (String, bool) {
    match input {
        Value::String(spec) => {
            let name = spec
                .split_whitespace()
                .find_map(|token| token.strip_prefix("name="))
                .map(|name| name.trim_matches(',').to_string())
                .unwrap_or_else(|| default_name.to_string());
            (
                name,
                spec.split_whitespace().any(|token| token == "required"),
            )
        }
        Value::Object(map) => {
            let name = map
                .get("valueName")
                .and_then(Value::as_str)
                .unwrap_or(default_name)
                .to_string();
            let required = map
                .get("valueRequired")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (name, required)
        }
        Value::Array(items) => items
            .iter()
            .map(|item| parse_input_spec(item, default_name))
            .find(|(name, required)| name != default_name || *required)
            .unwrap_or_else(|| (default_name.to_string(), false)),
        _ => (default_name.to_string(), false),
    }
}

fn placeholder_names(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        let Some(end) = remaining[start + 1..].find('}') else {
            break;
        };
        let expression = remaining[start + 1..start + 1 + end]
            .trim()
            .trim_start_matches(['?', '&', '+']);
        for name in expression
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.to_string());
            }
        }
        remaining = &remaining[start + end + 2..];
    }
    names
}

fn supported_url_template(template: &str) -> bool {
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        let Some(end) = remaining[start + 1..].find('}') else {
            return false;
        };
        let expression = remaining[start + 1..start + 1 + end].trim();
        if expression.is_empty() || expression.starts_with('+') {
            return false;
        }
        let variables = expression.trim_start_matches(['?', '&']);
        if variables.split(',').any(|name| name.trim().is_empty()) {
            return false;
        }
        remaining = &remaining[start + end + 2..];
    }
    !remaining.contains('}')
}

fn form_default_fields(form: &HtmlForm) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for field in &form.fields {
        if let Some(value) = &field.value {
            map.insert(field.name.clone(), value.clone());
        }
    }
    map
}

fn fields_to_schema(fields: &[FormField], skip_hidden: bool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in fields {
        if skip_hidden && field.hidden {
            continue;
        }
        let mut prop = Map::new();
        let json_type = match field.field_type.as_str() {
            "number" | "range" => "number",
            "checkbox" => "boolean",
            _ => "string",
        };
        prop.insert("type".into(), json!(json_type));
        if let Some(desc) = &field.description {
            prop.insert("description".into(), json!(desc));
        }
        if !field.options.is_empty() {
            let enums: Vec<Value> = field.options.iter().map(|(v, _)| json!(v)).collect();
            let any_of: Vec<Value> = field
                .options
                .iter()
                .map(|(v, title)| json!({"type": "string", "const": v, "title": title}))
                .collect();
            prop.insert("enum".into(), Value::Array(enums));
            prop.insert("anyOf".into(), Value::Array(any_of));
        }
        properties.insert(field.name.clone(), Value::Object(prop));
        if field.required {
            required.push(field.name.clone());
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required
    })
}

pub fn looks_like_llms_url(source_url: &str) -> bool {
    let path = match Url::parse(source_url) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => source_url.to_ascii_lowercase(),
    };
    let path = path.trim_end_matches('/');
    path.ends_with("llms.txt") || path.ends_with("llms-full.txt")
}

pub fn parse_llms_txt(source_url: &str, raw: &str) -> Option<LlmsTxtDocument> {
    let text = raw.trim_start_matches('\u{feff}').trim();
    if text.is_empty() || looks_like_html(text) {
        return None;
    }
    let mut lines = text.lines().peekable();
    let mut title = None;
    while let Some(line) = lines.peek() {
        let t = line.trim();
        if t.is_empty() {
            lines.next();
            continue;
        }
        if let Some(rest) = t.strip_prefix("# ") {
            title = Some(rest.trim().to_string());
            lines.next();
            break;
        }
        // Best-effort only for paths that are actually llms.txt.
        if looks_like_llms_url(source_url) {
            title = Some(t.trim_start_matches('#').trim().to_string());
            lines.next();
            break;
        }
        return None;
    }
    let title = title?;

    let mut summary = None;
    let mut details = Vec::new();
    let mut sections = Vec::new();
    let mut current: Option<LlmsSection> = None;

    for line in lines {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let name = rest.trim().to_string();
            current = Some(LlmsSection {
                optional: name.eq_ignore_ascii_case("optional"),
                name,
                links: Vec::new(),
            });
            continue;
        }
        if current.is_none() {
            if summary.is_none() {
                if let Some(inner) = trimmed.strip_prefix('>').map(|s| s.trim()) {
                    summary = Some(inner.to_string());
                    continue;
                }
            }
            if !trimmed.is_empty() {
                details.push(trimmed.to_string());
            }
            continue;
        }
        if let Some(section) = current.as_mut() {
            if let Some(link) = parse_md_link(trimmed) {
                section.links.push(link);
            }
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }

    Some(LlmsTxtDocument {
        source_url: source_url.to_string(),
        title,
        summary,
        details: if details.is_empty() {
            None
        } else {
            Some(details.join("\n"))
        },
        sections,
    })
}

fn parse_md_link(line: &str) -> Option<LlmsLink> {
    let line = line.trim().trim_start_matches(['-', '*', '+']).trim();
    let rest = line.strip_prefix('[')?;
    let end_name = rest.find(']')?;
    let title = rest[..end_name].to_string();
    let after = rest[end_name + 1..].trim();
    let after = after.strip_prefix('(')?;
    let end_url = after.find(')')?;
    let url = after[..end_url].to_string();
    let notes = after[end_url + 1..].trim().trim_start_matches(':').trim();
    Some(LlmsLink {
        title,
        url,
        notes: if notes.is_empty() {
            None
        } else {
            Some(notes.to_string())
        },
    })
}

fn looks_like_html(text: &str) -> bool {
    let t = text.trim_start().to_ascii_lowercase();
    t.starts_with("<!doctype html") || t.starts_with("<html")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    #[test]
    fn parses_declarative_webmcp_form() {
        let html = r#"<html><body>
          <form toolname="book_table" tooldescription="Reserve a table" toolautosubmit action="/book">
            <input type="hidden" name="csrf" value="token-abc">
            <label for="name">Full Name</label>
            <input id="name" name="name" required>
            <select name="guests" toolparamdescription="Party size" required>
              <option value="1">1 Person</option>
              <option value="2">2 People</option>
            </select>
          </form>
        </body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://bistro.example/").unwrap();
        let d = discover_in_html(&doc, &url);
        assert_eq!(d.tools.len(), 1);
        let tool = &d.tools[0];
        assert_eq!(tool.name, "book_table");
        assert_eq!(tool.kind, ToolKind::WebmcpDeclarative);
        assert!(tool.input_schema["properties"]["guests"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "2"));
        match &tool.invocation {
            ToolInvocation::Http {
                url,
                method,
                default_fields,
                ..
            } => {
                assert_eq!(method, "GET");
                assert!(url.ends_with("/book"));
                assert_eq!(
                    default_fields.get("csrf").map(String::as_str),
                    Some("token-abc")
                );
            }
            _ => panic!("expected http invocation"),
        }
        assert!(tool.autosubmit);
        assert!(tool.notes.iter().all(|n| !n.contains("toolautosubmit")));
    }

    #[test]
    fn missing_toolautosubmit_still_autosubmits() {
        let html = r#"<html><body>
          <form toolname="search" tooldescription="Search the catalog" action="/search" method="get">
            <input type="text" name="q" required>
          </form>
        </body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|t| t.kind == ToolKind::WebmcpDeclarative)
            .expect("declarative tool");
        assert!(tool.autosubmit);
        assert!(
            tool.notes
                .iter()
                .all(|n| !n.contains("human") && !n.contains("confirm")),
            "{:?}",
            tool.notes
        );
        match &tool.invocation {
            ToolInvocation::Http { url, method, .. } => {
                assert_eq!(method, "GET");
                assert!(url.ends_with("/search"));
            }
            other => panic!("expected HTTP invocation, got {other:?}"),
        }
    }

    #[test]
    fn detects_imperative_without_declarative() {
        let html = r#"<html><body>
          <script>navigator.modelContext.registerTool({name: "doThing"})</script>
        </body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        assert!(d.imperative_webmcp);
        assert!(!d
            .tools
            .iter()
            .any(|t| t.kind == ToolKind::WebmcpDeclarative));
    }

    #[test]
    fn parses_json_ld_search_action() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@type":"WebSite","potentialAction":{
            "@type":"SearchAction","target":"https://example.com/search?q={search_term_string}",
            "query-input":"required name=search_term_string"}}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        assert_eq!(d.json_ld_actions.len(), 1);
        let tool = d
            .tools
            .iter()
            .find(|t| t.kind == ToolKind::JsonLdAction)
            .expect("SearchAction tool");
        assert_eq!(tool.name, "search_action");
        match &tool.invocation {
            ToolInvocation::Http { method, url, .. } => {
                assert_eq!(method, "GET");
                assert!(url.contains("/search?q={search_term_string}"), "{url}");
            }
            other => panic!("expected HTTP invocation, got {other:?}"),
        }
    }

    #[test]
    fn parses_json_ld_search_action_post_entrypoint() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@type":"WebSite","potentialAction":{
            "@type":"SearchAction",
            "target":{
              "@type":"EntryPoint",
              "urlTemplate":"/find",
              "httpMethod":"POST",
              "encodingType":"application/x-www-form-urlencoded"
            },
            "query-input":"required name=search_term_string"
          }}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/docs").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|t| t.kind == ToolKind::JsonLdAction)
            .expect("SearchAction tool");
        assert_eq!(
            tool.input_schema["required"][0].as_str(),
            Some("search_term_string")
        );
        match &tool.invocation {
            ToolInvocation::Http {
                method,
                url,
                enctype,
                ..
            } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://example.com/find");
                assert_eq!(enctype, "application/x-www-form-urlencoded");
            }
            other => panic!("expected HTTP invocation, got {other:?}"),
        }
    }

    #[test]
    fn parses_reserve_action_as_post_tool() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@type":"Restaurant","potentialAction":{
            "@type":"ReserveAction",
            "name":"reserve_table",
            "target":{
              "@type":"EntryPoint",
              "urlTemplate":"/reservations",
              "httpMethod":"POST",
              "encodingType":"application/json"
            },
            "partySize-input":"required name=party_size",
            "startTime-input":{"@type":"PropertyValueSpecification","valueName":"start_time","valueRequired":true}
          }}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://restaurant.example/menu").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|tool| tool.name == "reserve_table")
            .expect("ReserveAction tool");
        assert_eq!(tool.kind, ToolKind::JsonLdAction);
        assert_eq!(
            tool.input_schema["required"],
            json!(["party_size", "start_time"])
        );
        match &tool.invocation {
            ToolInvocation::Http {
                method,
                url,
                enctype,
                ..
            } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://restaurant.example/reservations");
                assert_eq!(enctype, "application/json");
            }
            other => panic!("expected HTTP invocation, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_annotation_only_action_as_unavailable() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@type":"Restaurant","potentialAction":{
            "@type":"ReserveAction","target":"https://restaurant.example/book"
          }}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://restaurant.example/").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|tool| tool.name == "reserve_action")
            .expect("ReserveAction metadata");
        assert!(matches!(
            tool.invocation,
            ToolInvocation::Unavailable { .. }
        ));
    }

    #[test]
    fn supports_schema_compact_iri_action_types() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":{"schema":"https://schema.org/"},"@type":"schema:WebSite","potentialAction":{
            "@type":"schema:SearchAction",
            "target":"https://example.com/search?q={search_term_string}",
            "query-input":"required name=search_term_string"
          }}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|tool| tool.name == "search_action")
            .expect("compact-IRI SearchAction");
        assert!(matches!(tool.invocation, ToolInvocation::Http { .. }));
    }

    #[test]
    fn generated_action_names_are_unique() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@graph":[
            {"@type":"ReserveAction","name":"foo","target":{"url":"/one","httpMethod":"POST"}},
            {"@type":"ReserveAction","name":"foo_2","target":{"url":"/two","httpMethod":"POST"}},
            {"@type":"ReserveAction","name":"foo","target":{"url":"/three","httpMethod":"POST"}}
          ]}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        let names: Vec<_> = d
            .tools
            .iter()
            .filter(|tool| tool.kind == ToolKind::JsonLdAction)
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, vec!["foo", "foo_2", "foo_3"]);
    }

    #[test]
    fn unsupported_uri_template_is_not_invocable() {
        let html = r#"<html><head>
          <script type="application/ld+json">
          {"@context":"https://schema.org","@type":"Thing","potentialAction":{
            "@type":"ReserveAction",
            "target":{"urlTemplate":"https://example.com/{+path}","httpMethod":"POST"},
            "path-input":"required name=path"
          }}
          </script>
        </head><body><p>hi</p></body></html>"#;
        let doc = Html::parse_document(html);
        let url = Url::parse("https://example.com/").unwrap();
        let d = discover_in_html(&doc, &url);
        let tool = d
            .tools
            .iter()
            .find(|tool| tool.name == "reserve_action")
            .expect("ReserveAction");
        assert!(matches!(
            tool.invocation,
            ToolInvocation::Unavailable { .. }
        ));
    }

    #[test]
    fn parses_llms_txt() {
        let raw = "# FastHTML\n\n> A python library\n\nMore details here.\n\n## Docs\n\n- [Intro](https://example.com/intro.md): start here\n\n## Optional\n\n- [Extra](https://example.com/extra.md)\n";
        let doc = parse_llms_txt("https://example.com/llms.txt", raw).unwrap();
        assert_eq!(doc.title, "FastHTML");
        assert_eq!(doc.summary.as_deref(), Some("A python library"));
        assert_eq!(doc.sections.len(), 2);
        assert!(doc.sections[1].optional);
        assert_eq!(doc.sections[0].links[0].title, "Intro");
    }

    #[test]
    fn rejects_html_error_pages_as_llms_txt() {
        let raw = "<!doctype html><html><body>404</body></html>";
        assert!(parse_llms_txt("https://example.com/llms.txt", raw).is_none());
    }

    #[test]
    fn rejects_random_plaintext_as_llms_txt() {
        let raw = "body { margin: 0 }\n.foo { color: red; }";
        assert!(parse_llms_txt("https://example.com/style.css", raw).is_none());
        assert!(parse_llms_txt("https://example.com/readme.txt", "Hello world").is_none());
    }
}
