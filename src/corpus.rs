use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::classify::{classify, ClassificationSignals};
use crate::discover::discover_in_html;
use crate::envelope::CapabilityTier;
use crate::extract::extract_page;

#[derive(Debug, Deserialize)]
struct Manifest {
    fixtures: Vec<FixtureCase>,
    #[serde(default)]
    live: Vec<LiveCase>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    name: String,
    html: PathBuf,
    expected_tier: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LiveCase {
    name: String,
    url: String,
    expected_tier: String,
}

pub async fn run(manifest: &Path, live: bool) -> anyhow::Result<()> {
    let root = manifest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let text = fs::read_to_string(manifest)?;
    let manifest: Manifest = serde_json::from_str(&text)?;

    let mut failed = 0;
    let mut passed = 0;

    for case in &manifest.fixtures {
        let html_path = if case.html.is_absolute() {
            case.html.clone()
        } else {
            root.join(&case.html)
        };
        let html = fs::read_to_string(&html_path)?;
        let base = case
            .base_url
            .clone()
            .unwrap_or_else(|| "https://example.com/".into());
        let extracted = extract_page(&html, &base);
        let document = scraper::Html::parse_document(&html);
        let url = url::Url::parse(&base)?;
        let disc = discover_in_html(&document, &url);
        let signals = ClassificationSignals {
            has_webmcp_declarative: disc.forms.iter().any(|f| f.is_webmcp()),
            has_llms_txt: false,
            has_json_ld_actions: !disc.json_ld_actions.is_empty(),
            imperative_webmcp_only: disc.imperative_webmcp
                && !disc.forms.iter().any(|f| f.is_webmcp()),
        };
        let got = classify(&extracted, &signals, None);
        let expected = parse_expected(&case.expected_tier);
        if Some(got.tier) == expected {
            eprintln!("ok  fixture {:<32} {}", case.name, got.tier.as_str());
            passed += 1;
        } else {
            eprintln!(
                "FAIL fixture {:<32} expected {:?} got {} ({})",
                case.name,
                case.expected_tier,
                got.tier.as_str(),
                got.reason
            );
            failed += 1;
        }
    }

    if live {
        let client = crate::client::AgentNavigator::new(crate::config::ClientConfig::default())?;
        for case in &manifest.live {
            let env = client
                .navigate(crate::client::NavigateRequest {
                    url: case.url.clone(),
                    ignore_robots: false,
                    ..Default::default()
                })
                .await;
            let expected = parse_expected(&case.expected_tier);
            if Some(env.capability_tier) == expected {
                eprintln!(
                    "ok  live    {:<32} {}  {}ms",
                    case.name,
                    env.capability_tier.as_str(),
                    env.metadata.elapsed_ms
                );
                passed += 1;
            } else {
                eprintln!(
                    "FAIL live    {:<32} expected {} got {} ({})",
                    case.name,
                    case.expected_tier,
                    env.capability_tier.as_str(),
                    env.metadata.classification_reason
                );
                failed += 1;
            }
        }
    }

    eprintln!("{passed} passed, {failed} failed");
    if failed > 0 {
        anyhow::bail!("corpus failures: {failed}");
    }
    Ok(())
}

fn parse_expected(s: &str) -> Option<CapabilityTier> {
    crate::config::parse_tier(s)
}
