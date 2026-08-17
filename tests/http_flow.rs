use agent_navigator::client::{AgentNavigator, NavigateRequest};
use agent_navigator::config::ClientConfig;
use agent_navigator::envelope::{CapabilityTier, EnvelopeStatus, EscalationKind, ToolKind};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(session_dir: &std::path::Path) -> ClientConfig {
    ClientConfig {
        session_dir: session_dir.to_path_buf(),
        ignore_robots: true,
        ..ClientConfig::default()
    }
}

#[tokio::test]
async fn navigate_static_html_is_readable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read_to_string("corpus/fixtures/static_article.html").unwrap(),
            "text/html",
        ))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-test-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    assert!(env.is_ok(), "{:?}", env.status);
    assert_eq!(env.capability_tier, CapabilityTier::StaticReadable);
    let md = env.content.markdown.unwrap();
    assert!(md.contains("Ownership"), "{md}");
    assert!(md.contains("2021"), "{md}");
}

#[tokio::test]
async fn empty_spa_escalates_js_required() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read_to_string("corpus/fixtures/spa_empty.html").unwrap(),
            "text/html",
        ))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-spa-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    assert_eq!(env.capability_tier, CapabilityTier::JsRequired);
    match env.status {
        EnvelopeStatus::Escalation {
            escalation: EscalationKind::JsRequired,
            ..
        } => {}
        other => panic!("expected js-required escalation, got {other:?}"),
    }
    assert!(env.content.markdown.is_none() || env.content.markdown.as_deref() == Some(""));
}

#[tokio::test]
async fn webmcp_tool_is_discovered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            std::fs::read_to_string("corpus/fixtures/webmcp_form.html").unwrap(),
            "text/html",
        ))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-mcp-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    assert_eq!(env.capability_tier, CapabilityTier::Structured);
    assert!(env
        .metadata
        .capabilities
        .iter()
        .any(|c| c == "webmcp_declarative"));
    assert_eq!(env.tools_available[0].name, "book_table_le_petit_bistro");
}

#[tokio::test]
async fn robots_txt_blocks_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("should not fetch"))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-robots-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        ignore_robots: false,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: format!("{}/secret", server.uri()),
            ignore_robots: false,
            ..Default::default()
        })
        .await;
    match env.status {
        EnvelopeStatus::Escalation {
            escalation: EscalationKind::RobotsDisallowed,
            ..
        } => {}
        other => panic!("expected robots escalation, got {other:?}"),
    }
    assert_ne!(env.capability_tier, CapabilityTier::JsRequired);
}

#[tokio::test]
async fn robots_txt_blocks_redirect_targets() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("User-agent: *\nAllow: /ok\nDisallow: /secret\n"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/secret"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("leaked"))
        .expect(0)
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-robots-redir-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        ignore_robots: false,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: format!("{}/ok", server.uri()),
            ignore_robots: false,
            ..Default::default()
        })
        .await;
    match env.status {
        EnvelopeStatus::Escalation {
            escalation: EscalationKind::RobotsDisallowed,
            ..
        } => {}
        other => panic!("expected robots escalation, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_form_sends_hidden_csrf() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"<!doctype html><html><body>
              <form id="login" action="/login" method="post">
                <input type="hidden" name="csrf" value="token-abc">
                <input name="user">
              </form>
            </body></html>"#,
            "text/html",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(wiremock::matchers::body_string_contains("csrf=token-abc"))
        .and(wiremock::matchers::body_string_contains("user=ada"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><body><article><p>Welcome ada, you are logged in and this page has enough words to classify as static readable content for the agent.</p></article></body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-csrf-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .submit_form(
            &server.uri(),
            Some("login"),
            serde_json::json!({"user": "ada"}),
            None,
            true,
        )
        .await;
    assert!(env.is_ok(), "{:?}", env.status);
    let md = env.content.markdown.unwrap_or_default();
    assert!(md.contains("Welcome ada"), "{md}");
}

#[tokio::test]
async fn submit_form_preserves_robots_denial() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n"))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-form-robots-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        ignore_robots: false,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .submit_form(
            &format!("{}/login", server.uri()),
            None,
            serde_json::json!({}),
            None,
            false,
        )
        .await;
    match env.status {
        EnvelopeStatus::Escalation {
            escalation: EscalationKind::RobotsDisallowed,
            ..
        } => {}
        other => panic!("expected robots escalation, got {other:?}"),
    }
}

#[tokio::test]
async fn plaintext_is_not_llms_txt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/readme.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("Hello world\nThis is a readme.", "text/plain"),
        )
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-txt-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: format!("{}/readme.txt", server.uri()),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    assert_eq!(env.capability_tier, CapabilityTier::StaticReadable);
    assert!(!env.metadata.capabilities.iter().any(|c| c == "llms_txt"));
}

#[tokio::test]
async fn rejects_file_scheme_and_bad_session_name() {
    let tmp = std::env::temp_dir().join(format!("af-scheme-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: "file:///etc/passwd".into(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    match env.status {
        EnvelopeStatus::Error { code, .. } => assert_eq!(code, "forbidden_url"),
        other => panic!("expected forbidden_url, got {other:?}"),
    }

    let env = client
        .navigate(NavigateRequest {
            url: "https://example.com/".into(),
            session: Some("../evil".into()),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    match env.status {
        EnvelopeStatus::Error { code, .. } => assert_eq!(code, "session_error"),
        other => panic!("expected session_error, got {other:?}"),
    }
}

#[tokio::test]
async fn call_jsonld_action_gets_url_template() {
    let server = MockServer::start().await;
    let page = format!(
        r#"<!doctype html><html><head>
          <script type="application/ld+json">
          {{"@context":"https://schema.org","@type":"WebSite","potentialAction":{{
            "@type":"SearchAction",
            "target":"{}/search?q={{search_term_string}}",
            "query-input":"required name=search_term_string"
          }}}}
          </script>
        </head><body><main><h1>Docs</h1>
        <p>Search the documentation catalog using the site-declared SearchAction. This page is ordinary static HTML with enough prose that a fetch client can extract it without running any scripts.</p>
        </main></body></html>"#,
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><body><article><h1>Results</h1><p>Found rust crates and this page has enough words to classify as static readable content for the agent.</p></article></body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-search-get-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let env = client
        .call_jsonld_action(
            &server.uri(),
            "search_action",
            serde_json::json!({"search_term_string": "rust"}),
            None,
            true,
        )
        .await;
    assert!(env.is_ok(), "{:?}", env.status);
    assert!(
        !env.metadata.final_url.contains("search_term_string"),
        "placeholder name should not be appended as a second query param: {}",
        env.metadata.final_url
    );
    let md = env.content.markdown.unwrap_or_default();
    assert!(md.contains("Found rust"), "{md}");
}

#[tokio::test]
async fn call_jsonld_action_posts_reserve_entrypoint() {
    let server = MockServer::start().await;
    let page = format!(
        r#"<!doctype html><html><head>
          <script type="application/ld+json">
          {{"@context":"https://schema.org","@type":"Restaurant","potentialAction":{{
            "@type":"ReserveAction",
            "name":"reserve_table",
            "target":{{
              "@type":"EntryPoint",
              "urlTemplate":"{}/reservations",
              "httpMethod":"POST",
              "encodingType":"application/json"
            }},
            "partySize-input":"required name=party_size"
          }}}}
          </script>
        </head><body><main><h1>Restaurant</h1>
        <p>Reserve a table using the site-declared ReserveAction. This page is ordinary static HTML with enough prose that a fetch client can extract it without running any scripts.</p>
        </main></body></html>"#,
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/reservations"))
        .and(body_json(serde_json::json!({"party_size": 4})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><html><body><article><h1>Reserved</h1><p>Reserved a table for four people and this page has enough words to classify as static readable content for the agent.</p></article></body></html>",
            "text/html",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-search-post-{}", std::process::id()));
    let client = AgentNavigator::new(config(&tmp)).unwrap();
    let discovered = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    let tool = discovered
        .tools_available
        .iter()
        .find(|t| t.kind == ToolKind::JsonLdAction)
        .expect("ReserveAction");
    match &tool.invocation {
        agent_navigator::envelope::ToolInvocation::Http { method, .. } => {
            assert_eq!(method, "POST");
        }
        other => panic!("expected HTTP invocation, got {other:?}"),
    }

    let invalid = client
        .call_jsonld_action(
            &server.uri(),
            "reserve_table",
            serde_json::json!({}),
            None,
            true,
        )
        .await;
    match invalid.status {
        EnvelopeStatus::Error { code, .. } => assert_eq!(code, "invalid_tool_arguments"),
        other => panic!("expected invalid arguments error, got {other:?}"),
    }

    let env = client
        .call_jsonld_action(
            &server.uri(),
            "reserve_table",
            serde_json::json!({"party_size": 4}),
            None,
            true,
        )
        .await;
    assert!(env.is_ok(), "{:?}", env.status);
    let md = env.content.markdown.unwrap_or_default();
    assert!(md.contains("Reserved a table"), "{md}");
}
