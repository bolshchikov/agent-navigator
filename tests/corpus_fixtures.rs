use std::path::PathBuf;

use agent_navigator::classify::{classify, ClassificationSignals};
use agent_navigator::discover::discover_in_html;
use agent_navigator::envelope::CapabilityTier;
use agent_navigator::extract::extract_page;

#[test]
fn fixture_corpus_classifies_as_expected() {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/manifest.json");
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for case in v["fixtures"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let html_rel = case["html"].as_str().unwrap();
        let expected = case["expected_tier"].as_str().unwrap();
        let html = std::fs::read_to_string(manifest_path.parent().unwrap().join(html_rel)).unwrap();
        let extracted = extract_page(&html, "https://example.com/");
        let document = scraper::Html::parse_document(&html);
        let url = url::Url::parse("https://example.com/").unwrap();
        let disc = discover_in_html(&document, &url);
        let signals = ClassificationSignals {
            has_webmcp_declarative: disc.forms.iter().any(|f| f.is_webmcp()),
            has_llms_txt: false,
            has_json_ld_actions: !disc.json_ld_actions.is_empty(),
            imperative_webmcp_only: disc.imperative_webmcp
                && !disc.forms.iter().any(|f| f.is_webmcp()),
        };
        let got = classify(&extracted, &signals, None);
        let exp = match expected {
            "structured" => CapabilityTier::Structured,
            "static-readable" => CapabilityTier::StaticReadable,
            "js-required" => CapabilityTier::JsRequired,
            other => panic!("unknown tier {other}"),
        };
        assert_eq!(got.tier, exp, "{name}: {}", got.reason);
    }
}
