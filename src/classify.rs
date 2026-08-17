use crate::envelope::CapabilityTier;
use crate::extract::ExtractedPage;

#[derive(Debug, Clone)]
pub struct Classification {
    pub tier: CapabilityTier,
    pub reason: String,
    pub overridden: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ClassificationSignals {
    pub has_webmcp_declarative: bool,
    pub has_llms_txt: bool,
    pub has_json_ld_actions: bool,
    pub imperative_webmcp_only: bool,
}

pub fn classify(
    extracted: &ExtractedPage,
    signals: &ClassificationSignals,
    override_tier: Option<CapabilityTier>,
) -> Classification {
    if let Some(tier) = override_tier {
        return Classification {
            reason: format!(
                "per-domain override set capability_tier to {}",
                tier.as_str()
            ),
            tier,
            overridden: true,
        };
    }

    let mut reasons = Vec::new();
    let structured =
        signals.has_webmcp_declarative || signals.has_llms_txt || signals.has_json_ld_actions;

    if signals.has_webmcp_declarative {
        reasons.push("WebMCP declarative tool(s) present in HTML".to_string());
    }
    if signals.has_llms_txt {
        reasons.push("llms.txt (or llms-full.txt) discovered".to_string());
    }
    if signals.has_json_ld_actions {
        reasons.push("JSON-LD schema.org Action markup present".to_string());
    }

    if structured {
        if extracted.spa_shell.is_some() || extracted.enable_js_hint {
            reasons.push(
                "page also shows SPA/JS hints; structured surfaces are still usable without JS"
                    .to_string(),
            );
        }
        return Classification {
            tier: CapabilityTier::Structured,
            reason: reasons.join("; "),
            overridden: false,
        };
    }

    if signals.imperative_webmcp_only {
        return Classification {
            tier: CapabilityTier::JsRequired,
            reason: "WebMCP Imperative API detected (navigator.modelContext.registerTool) with no declarative tools; JavaScript execution is required".to_string(),
            overridden: false,
        };
    }

    if let Some(shell) = &extracted.spa_shell {
        if extracted.word_count < 40 {
            return Classification {
                tier: CapabilityTier::JsRequired,
                reason: format!(
                    "{shell}; visible text is too thin ({words} words) to treat as static HTML",
                    words = extracted.word_count
                ),
                overridden: false,
            };
        }
    }

    if extracted.enable_js_hint && extracted.word_count < 80 {
        return Classification {
            tier: CapabilityTier::JsRequired,
            reason: "page asks for JavaScript and extractable content is thin".to_string(),
            overridden: false,
        };
    }

    if extracted.word_count < 20 && extracted.script_count >= 2 {
        return Classification {
            tier: CapabilityTier::JsRequired,
            reason: format!(
                "near-empty body ({} words) with {} script tags — content is likely injected at runtime",
                extracted.word_count, extracted.script_count
            ),
            overridden: false,
        };
    }

    if extracted.word_count < 12 && extracted.markdown.split_whitespace().count() < 12 {
        return Classification {
            tier: CapabilityTier::JsRequired,
            reason: format!(
                "insufficient extractable content ({} visible words, markdown ~{} words)",
                extracted.word_count,
                extracted.markdown.split_whitespace().count()
            ),
            overridden: false,
        };
    }

    Classification {
        tier: CapabilityTier::StaticReadable,
        reason: format!(
            "no structured agent surface; {} words of extractable HTML/markdown in the initial response",
            extracted.word_count.max(extracted.markdown.split_whitespace().count())
        ),
        overridden: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::extract_page;

    fn page(html: &str) -> ExtractedPage {
        extract_page(html, "https://example.com")
    }

    #[test]
    fn spa_empty_root_is_js_required() {
        let html = r#"<html><body><div id="root"></div>
            <script src="/app.js"></script><script src="/chunk.js"></script></body></html>"#;
        let c = classify(&page(html), &ClassificationSignals::default(), None);
        assert_eq!(c.tier, CapabilityTier::JsRequired);
        assert!(c.reason.contains("#root"), "{}", c.reason);
    }

    #[test]
    fn next_ssr_with_content_is_static() {
        let html = r#"<html><body><div id="__next">
            <article><h1>Server rendered</h1>
            <p>This Next.js page shipped a full article in the initial HTML so an agent can read it without executing JavaScript at all.</p>
            <p>A second paragraph keeps the word count honestly above the thin-content threshold used by the classifier.</p>
            </article>
            </div></body></html>"#;
        let c = classify(&page(html), &ClassificationSignals::default(), None);
        assert_eq!(c.tier, CapabilityTier::StaticReadable, "{}", c.reason);
    }

    #[test]
    fn webmcp_promotes_to_structured() {
        let html = r#"<html><body><div id="root"></div>
            <form toolname="search" tooldescription="Search the catalog">
              <input name="q"/>
            </form>
            <script src="app.js"></script></body></html>"#;
        let signals = ClassificationSignals {
            has_webmcp_declarative: true,
            ..Default::default()
        };
        let c = classify(&page(html), &signals, None);
        assert_eq!(c.tier, CapabilityTier::Structured);
    }

    #[test]
    fn override_wins() {
        let html = r#"<html><body><div id="root"></div><script src="a.js"></script><script src="b.js"></script></body></html>"#;
        let c = classify(
            &page(html),
            &ClassificationSignals::default(),
            Some(CapabilityTier::StaticReadable),
        );
        assert_eq!(c.tier, CapabilityTier::StaticReadable);
        assert!(c.overridden);
    }
}
