# PRD: Agent-Native Web Client ("AgentNavigator")

**Status:** Draft v1
**Owner:** Sergey
**Last updated:** August 15, 2026

---

## 1. Summary

AgentNavigator is a Rust-based HTTP client purpose-built for AI agents to navigate the web. It is not a browser: it never executes JavaScript or renders a DOM. Instead, it treats the web as a set of structured, agent-declared surfaces — WebMCP tools, llms.txt, JSON-LD/schema.org actions, and readable static HTML — and exposes them to any agent as a small set of typed operations (`navigate`, `extract`, `discover`, `call_webmcp_tool`, `submit_form`, and `call_jsonld_action`).

It sits in the gap between two existing extremes:
- **Dumb fetch tools** (Anthropic's `web_fetch`, curl, etc.) — return raw/static HTML only, no interaction, no structured discovery.
- **Full browser automation** (Claude in Chrome, Computer Use, Vercel `agent-browser`) — render real DOMs, execute JS, click/type like a human; powerful but heavy, slow, and unnecessary when a site has already exposed a machine-readable interface.

AgentNavigator is deliberately narrow: it only works well on sites that meet it partway (structured data, SSR'd content, or WebMCP declarative tools). Sites that require JS execution are explicitly out of scope — AgentNavigator should detect this and say so, not attempt to fake it.

---

## 2. Problem Statement

Agent developers currently choose between two bad tradeoffs:

1. Use a plain fetch tool and get garbage on any JS-heavy site (empty shells, loading skeletons).
2. Use full browser automation for every request, paying the cost of a browser process, screenshots or accessibility-tree snapshots, session management, and much higher latency/token overhead — even for sites that would happily hand over structured data if asked correctly.

Meanwhile, a growing set of standards (WebMCP, llms.txt, JSON-LD Action markup) exist specifically to let sites declare agent-usable interfaces — but there is no dedicated, lightweight client that treats these as first-class citizens instead of as an afterthought bolted onto a browser automation tool.

**There is no tool today whose entire design center is: "assume the site wants to be readable by an agent, and make that path as cheap and reliable as possible."**

---

## 3. Goals

- G1: Provide a single client that can navigate, read, and act on any site with GET/POST-level interactivity, without a rendering engine.
- G2: Make WebMCP (declarative), llms.txt, and JSON-LD/schema.org discovery first-class, zero-config capabilities.
- G3: Produce clean, LLM-ready output (markdown/structured JSON) from arbitrary static/SSR'd HTML as a fallback.
- G4: Explicitly and cheaply detect when a site requires JS execution or imperative WebMCP registration, and hand off rather than silently fail.
- G5: Expose the whole thing as an MCP server so it's a drop-in tool for any agent stack (including `deepagents`).
- G6: Be fast and cheap enough to be the default first attempt before escalating to a real browser agent.

### Non-Goals

- NG1: No JavaScript execution or DOM rendering, ever. This is a hard architectural boundary, not a phase-1 limitation.
- NG2: No attempt to defeat anti-bot/anti-automation systems (CAPTCHA solving, TLS fingerprint spoofing to impersonate a real browser, etc.).
- NG3: No visual/screenshot-based understanding of pages.
- NG4: No general-purpose web scraping/crawling product (no scheduling, no large-scale crawl orchestration) — this is a per-request agent tool, not a crawler.
- NG5: No built-in LLM — AgentNavigator returns data/content; the calling agent does the reasoning.

---

## 4. Target Users

- **Primary:** Developers building LLM agents that need to read and lightly act on the web (research agents, RevOps/data agents, shopping/comparison agents, form-submission agents) where most target sites are cooperative (SSR'd, structured, or WebMCP-enabled).
- **Secondary:** Anthropic-ecosystem builders (Claude Code / MCP users) who want a cheaper default than full browser automation for the majority of fetches, reserving Computer Use / Claude in Chrome for the JS-required minority.
- **Personal use case:** Sergey's own `deepagents` orchestration system and agent-friendly marketplace project — both want a shared, reusable "agent-native fetch layer" instead of building bespoke fetch logic twice.

---

## 5. Key User Stories

1. As an agent, I want to hit a URL and get back clean, structured content (markdown + metadata) in one call, so I don't need to write my own HTML-to-text pipeline.
2. As an agent, I want to discover and call a site's declared WebMCP tools directly, so I can take actions (search, add-to-cart, submit) without guessing at form fields.
3. As an agent, I want a reliable signal when a site cannot be handled without JS, so I can escalate to a browser-automation tool instead of silently getting empty/broken content.
4. As a developer, I want to plug AgentNavigator into my existing MCP-based agent with zero custom glue code.
5. As a developer, I want session/cookie state to persist across a sequence of calls (e.g., login → browse → submit) without re-authenticating each time.
6. As a developer, I want to respect robots.txt and rate limits by default, so I'm not building something that gets my agents blocked or is ethically dubious out of the box.

---

## 6. Functional Requirements

### 6.1 Core HTTP Layer
- FR1: Support GET, POST, PUT, PATCH, DELETE with custom headers, query params, and JSON/form bodies.
- FR2: Persistent cookie jar per session; sessions are named/isolated (mirrors `agent-browser`'s session model).
- FR3: Automatic redirect following with a configurable cap; redirect chain returned in metadata for debugging.
- FR4: Configurable timeout, retry-with-backoff policy for transient failures (5xx, timeouts).
- FR5: Respect `robots.txt` by default; explicit opt-out flag for cases where the operator has other authorization (documented, not silent).

### 6.2 Discovery Layer
On first contact with a domain, attempt in priority order (short-circuit on first strong hit, but cache the full result):
- FR6: Fetch and parse `/llms.txt` and `/llms-full.txt` if present.
- FR7: Detect WebMCP **declarative** tool manifests/attributes in HTML (per current WebMCP spec — this evolves, so the parser must be spec-version-aware and degrade gracefully on unknown attributes).
- FR8: Parse JSON-LD blocks for `schema.org` types, especially `Action` subtypes (SearchAction, OrderAction, etc.) and OpenGraph tags.
- FR9: Cache discovery results per domain with a TTL (default 24h, configurable) to avoid re-discovering on every call.
- FR10: Expose discovery results to the calling agent as structured metadata (`capabilities: ["llms_txt", "webmcp_declarative", "json_ld_actions"]`) so the agent can decide how to proceed.

### 6.3 Content Extraction
- FR11: HTML → clean markdown conversion using a readability-style algorithm (strip nav/ads/boilerplate) as the default fallback when no structured data exists.
- FR12: Preserve tables, lists, and links in extracted markdown (not just prose).
- FR13: Return raw HTML as an option for callers who want to do their own parsing.
- FR14: Extract and return page metadata separately from content: title, canonical URL, meta description, language, last-modified if available.

### 6.4 Capability Classification
- FR15: Every fetched page gets tagged as one of: `structured` (WebMCP/llms.txt/JSON-LD present), `static-readable` (no structured data but content is present in initial HTML), or `js-required` (heuristics: near-empty `<body>`, heavy reliance on `<script>`-injected root divs like `#root`/`#app`, absence of expected content markers).
- FR16: `js-required` responses return a clear, structured "escalate" signal (not an error, not silently empty content) including the reason for the classification, so the calling agent/orchestrator can hand off to a browser-automation tool.
- FR17: Classification heuristics must be tunable/overridable per-domain (allow/deny lists) since heuristics will misfire on edge cases (e.g., hydration-heavy SSR sites that are actually fine).

### 6.5 WebMCP Interaction (Declarative only)
- FR18: Parse declarative WebMCP tool definitions (form/element-based, per spec) into a normalized tool schema.
- FR19: Allow the calling agent to invoke a discovered WebMCP tool by name with arguments; AgentNavigator translates this into the appropriate HTTP request(s).
- FR20: Explicitly do NOT attempt to support the WebMCP **Imperative API** (`navigator.modelContext.registerTool`), since it requires JS execution — document this boundary clearly in tool output so agents don't assume full WebMCP coverage.

### 6.6 Forms (non-WebMCP fallback)
- FR21: Parse standard HTML `<form>` elements (action, method, fields, enctype) even on sites without WebMCP, and allow the agent to submit them with provided field values.
- FR22: Handle basic CSRF token patterns where the token is present as a static hidden field in the HTML (no JS-generated tokens — out of scope per NG1).

### 6.7 Session & Auth
- FR23: Support form-based login flows where credentials are supplied by the caller and posted to a discovered login form.
- FR24: Support bearer-token/API-key auth passed through as headers for sites/APIs that use them.
- FR25: Explicitly out of scope: OAuth flows requiring a JS-driven consent UI, magic-link flows requiring email interaction, MFA. Return a clear "auth-requires-browser" signal rather than failing ambiguously.

### 6.8 Agent-Facing Interface
- FR26: Ship as an MCP server (stdio and/or HTTP transport) exposing tools: `navigate`, `extract`, `discover`, `call_webmcp_tool`, `submit_form`, and `call_jsonld_action`. JSON-LD Actions are returned dynamically in `tools_available`; callers invoke them through `call_jsonld_action` using the declaring page URL, action name, and arguments.
- FR27: Also ship as a plain CLI (mirrors `agent-browser`'s pattern) for agents/frameworks that shell out rather than speak MCP.
- FR28: All responses are structured JSON with a consistent envelope: `{ status, capability_tier, content, metadata, tools_available }`.

---

## 7. Non-Functional Requirements

- NFR1: **Latency** — median request (discovery cached) should complete well under 1s for typical pages; this is a key differentiator vs. spinning up a browser context.
- NFR2: **Token efficiency** — output should be materially smaller/cleaner than raw HTML or full accessibility-tree dumps; target markdown output that's a fraction of raw HTML size.
- NFR3: **Safety** — must not be usable as a generic anti-bot-evasion tool; no user-agent spoofing to impersonate a real browser beyond honest identification (this is both an ethical and durability decision — the tool should announce itself, not disguise itself).
- NFR4: **Reliability** — clear, typed error/escalation states; no silent partial failures.
- NFR5: **Extensibility** — discovery/parsing layers should be modular enough to track evolving specs (WebMCP is still an origin trial as of mid-2026 and will change).
- NFR6: **Resource footprint** — no headless browser process, no GPU/rendering dependency; should run comfortably in constrained environments (serverless, containers).

---

## 8. Proposed Architecture

```
                    ┌─────────────────────────┐
                    │   Agent-Facing Layer     │
                    │  (MCP server / CLI)      │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────▼─────────────┐
                    │   Orchestration Core       │
                    │  (request routing, cache,  │
                    │   session mgmt, retries)   │
                    └──┬──────────┬───────────┬──┘
                       │          │           │
           ┌───────────▼──┐ ┌─────▼─────┐ ┌───▼────────────┐
           │ HTTP Client   │ │ Discovery │ │ Extraction      │
           │ (reqwest +    │ │ Layer     │ │ Layer           │
           │  cookie_store)│ │(llms.txt, │ │ (readability +  │
           │               │ │ WebMCP,   │ │  html5ever/     │
           │               │ │ JSON-LD)  │ │  scraper)       │
           └───────────────┘ └───────────┘ └─────────────────┘
                       │
           ┌───────────▼─────────────┐
           │ Capability Classifier    │
           │ (structured / static /   │
           │  js-required)            │
           └───────────────────────────┘
```

### Suggested crates
- `reqwest` + `cookie_store` — HTTP + session persistence
- `scraper` / `html5ever` — HTML parsing
- `serde` / `serde_json` — data interchange
- `robotstxt` — robots.txt compliance
- A Rust readability port (or a hand-rolled boilerplate-stripping heuristic) for markdown extraction
- `rmcp` or equivalent for the MCP server transport layer

---

## 9. Success Metrics

- % of test-suite URLs (mix of SSR sites, WebMCP-enabled sites, JS-heavy SPAs) correctly classified into the right capability tier.
- Median latency per request vs. baseline browser-automation tool (target: 5–10x faster for cooperative sites).
- Token count of extracted output vs. raw HTML (target: >80% reduction).
- False "js-required" escalation rate on known-good static/SSR sites (should trend toward zero as heuristics mature).
- Successful WebMCP tool-call rate on a curated set of WebMCP-enabled test sites.

---

## 10. Risks & Open Questions

| Risk | Notes |
|---|---|
| WebMCP spec is still an origin trial (Chrome 149, mid-2026) | Spec will change; discovery/parsing layer needs versioning and graceful degradation, not a hard-coded assumption. |
| Anti-bot detection blocks non-browser clients regardless of good behavior | Some sites will reject AgentNavigator purely on TLS/header fingerprint. Decide explicitly: is evading this in scope (NFR3 says no) — accept the miss rate as a cost of the "honest agent" design. |
| Heuristic misclassification (static vs. js-required) | Needs a real test corpus across many site archetypes, not just happy-path examples. Plan for an allow/deny override list from day one (FR17). |
| Auth long tail | Most real-world value-add sites (logged-in dashboards, SaaS tools) will have JS-driven auth. Scope early adopters carefully — internal tools/admin consoles with static login forms are a better initial wedge than consumer SaaS. |
| Overlap with `agent-browser` and Computer Use | Need a clear, documented handoff contract: AgentNavigator tries first, escalates cleanly with enough context that the browser tool doesn't have to start from scratch (e.g., pass along cookies/session state where possible). |
| Legal/ethical scraping boundaries | robots.txt compliance is a floor, not a full answer — consider a lightweight per-domain rate limiter by default. |

**Open questions for you to decide before Phase 1:**
1. Is the initial wedge your own two projects (`deepagents`, the marketplace), or do you want this to be a standalone open-source tool from day one?
2. Should session/auth state be shareable with a downstream browser-automation tool (for clean escalation), or is that a v2 concern?
3. How strict should the "no anti-bot evasion" stance be — e.g., is a realistic-but-honest User-Agent string acceptable, or should it always self-identify as an agent client?

---

## 11. Suggested Phasing (high level — happy to break this down further)

- **Phase 1:** HTTP core + readability extraction + MCP server shell. Get a working "smart fetch" that beats raw `web_fetch` on static/SSR sites.
- **Phase 2:** Discovery layer (llms.txt, JSON-LD) + capability classifier + clean js-required escalation signal.
- **Phase 3:** WebMCP declarative parsing + tool invocation.
- **Phase 4:** Forms/session/auth support for the static-login-form subset.
- **Phase 5:** Hardening — rate limiting, robots.txt, caching, CLI polish, docs.

---
*This PRD is a starting point — happy to go deeper on any section, or run it through the prd-phasing workflow to break Phase 1 into concrete tickets.*