# AgentNavigator

Rust CLI + MCP server that treats the web as a set of **agent-declared surfaces** — WebMCP (declarative), llms.txt, JSON-LD Actions, and readable static/SSR HTML. It is **not a browser**: it never executes JavaScript and never impersonates one.

## Boundaries

- No JS execution, no DOM rendering, no Playwright/Chrome fallback.
- No anti-bot evasion. The client self-identifies:
  `AgentNavigator/0.1.0 (+https://github.com/bolshchikov/agent-navigator; agent-native HTTP client; no JavaScript)`
- WebMCP **Declarative API only**. Imperative `navigator.modelContext.registerTool` is detected and reported as `webmcp-imperative-only` — it is not half-supported.
- Missing `toolautosubmit` is treated as **on**: `call_webmcp_tool` issues the HTTP submit. Chrome would wait for a human; this client has no confirmation UI.
- `robots.txt` is respected by default (including redirect hops). Bypass is `--ignore-robots` only.
- Only `http`/`https` URLs. Link-local and cloud-metadata addresses are refused. Session names are `[A-Za-z0-9_-]{1,64}`.

## Envelope

Every command returns:

```json
{ "status", "capability_tier", "content", "metadata", "tools_available" }
```

`capability_tier` is one of `structured` | `static-readable` | `js-required`. A `js-required` result is an **escalation signal**, not an empty successful fetch.

## CLI

```bash
cargo run -- navigate https://example.com/
cargo run -- discover https://llmstxt.org/
cargo run -- extract https://en.wikipedia.org/wiki/Rust_(programming_language)
cargo run -- mcp          # stdio MCP server (local Cursor / Claude Code)
cargo run -- mcp --http   # Streamable HTTP MCP at /mcp (bind 0.0.0.0:$PORT)
cargo run -- corpus       # fixture corpus
cargo run -- corpus --live
```

`--ignore-robots` is an explicit, documented opt-in. `--force-tier` and `--overrides overrides.json` cover FR17 misclassification.

## MCP tools

`navigate`, `extract`, `discover`, `call_webmcp_tool`, `submit_form`, `call_jsonld_action`

`navigate` and `discover` convert schema.org `*Action` markup into entries in
`tools_available`. Invoke an action with its declaring page URL, returned name,
and arguments. Actions without a concrete HTTP `EntryPoint` remain visible as
non-invocable metadata.

## Hosting (Render)

This is a CLI + MCP server, not a website. A Render **Web Service** needs Streamable HTTP:

```bash
agent-navigator mcp --http --allowed-host your-service.onrender.com
```

- MCP URL: `https://<service>.onrender.com/mcp`
- Health: `GET /health`
- Docker image in `Dockerfile`; Blueprint in `render.yaml` (Starter plan + 1 GB disk at `/var/data`).
- Set `MCP_ALLOWED_HOSTS` to the onrender hostname (and any custom domain). `RENDER_EXTERNAL_HOSTNAME` is picked up automatically when present.

The public demo is an outbound HTTP client. It refuses loopback/private URLs, ignores `ignore_robots` / caller headers / `include_html`, rate-limits `/mcp`, and namespaces cookie jars per MCP session. Do not send secrets to a public instance. Free-tier spin-down will drop MCP sessions; use **Starter**.

Local stdio MCP is unchanged: `cargo run -- mcp`.
