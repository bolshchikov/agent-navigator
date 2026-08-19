use agent_navigator::client::{AgentNavigator, NavigateRequest};
use agent_navigator::config::{ClientConfig, UrlPolicy};
use agent_navigator::envelope::{EnvelopeStatus, EscalationKind};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn public_policy_rejects_loopback_mock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html><body>hi</body></html>", "text/html"),
        )
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-public-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        url_policy: UrlPolicy::PublicOnly,
        allow_ignore_robots: true,
        ignore_robots: true,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            ..Default::default()
        })
        .await;
    match env.status {
        EnvelopeStatus::Error { code, .. } => assert_eq!(code, "forbidden_url"),
        other => panic!("expected forbidden_url, got {other:?}"),
    }
}

#[tokio::test]
async fn public_http_demo_flags() {
    let cfg = ClientConfig::default().public_http_demo();
    assert_eq!(cfg.url_policy, UrlPolicy::PublicOnly);
    assert!(!cfg.allow_ignore_robots);
    assert!(!cfg.allow_custom_headers);
    assert!(!cfg.allow_include_html);
}

#[tokio::test]
async fn allow_include_html_false_drops_raw_html() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html><body>hi</body></html>", "text/html"),
        )
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-nohtml-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        ignore_robots: true,
        allow_include_html: false,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
            include_html: true,
            ..Default::default()
        })
        .await;
    assert!(env.content.html.is_none());
}

#[tokio::test]
async fn allow_ignore_robots_false_still_consults_robots() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n"))
        .mount(&server)
        .await;

    let tmp = std::env::temp_dir().join(format!("af-norobots-{}", std::process::id()));
    let cfg = ClientConfig {
        session_dir: tmp,
        ignore_robots: false,
        allow_ignore_robots: false,
        ..ClientConfig::default()
    };
    let client = AgentNavigator::new(cfg).unwrap();
    let env = client
        .navigate(NavigateRequest {
            url: server.uri(),
            ignore_robots: true,
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
