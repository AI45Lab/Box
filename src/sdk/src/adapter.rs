//! Execution adapters for different workload types
//!
//! Adapters provide pluggable execution backends. The HTTP adapter is
//! the default for simple HTTP-based workloads.

use async_trait::async_trait;
use scraper::Selector;
use std::time::Duration;

/// Capability access level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityAccess {
    Read,
    Write,
    Execute,
}

/// Capability risk level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilityRisk {
    Low,
    Medium,
    High,
}

/// Execution capability
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionCapability {
    Network {
        protocol: String,
        operation: String,
        scope: String,
    },
    Filesystem {
        scope: String,
        access: CapabilityAccess,
    },
    Tool {
        name: String,
        access: CapabilityAccess,
        scope: String,
    },
}

/// Capability grant with risk assessment
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionCapabilityGrant {
    pub capability: ExecutionCapability,
    pub risk: CapabilityRisk,
}

/// Trait for execution adapters that can run workloads.
#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    /// Returns the capabilities this adapter grants.
    fn capabilities(&self) -> Vec<ExecutionCapabilityGrant>;

    /// Execute a workload with the given handler and input.
    async fn execute(
        &self,
        handler: &str,
        input: &serde_json::Value,
        timeout: Duration,
    ) -> crate::Result<serde_json::Value>;
}

/// HTTP execution adapter for web-based workloads.
pub struct HttpExecutionAdapter;

#[async_trait]
impl ExecutionAdapter for HttpExecutionAdapter {
    fn capabilities(&self) -> Vec<ExecutionCapabilityGrant> {
        vec![
            ExecutionCapabilityGrant {
                capability: ExecutionCapability::Network {
                    protocol: "http".into(),
                    operation: "fetch".into(),
                    scope: "public".into(),
                },
                risk: CapabilityRisk::Low,
            },
            ExecutionCapabilityGrant {
                capability: ExecutionCapability::Network {
                    protocol: "http".into(),
                    operation: "post".into(),
                    scope: "public".into(),
                },
                risk: CapabilityRisk::Medium,
            },
            ExecutionCapabilityGrant {
                capability: ExecutionCapability::Network {
                    protocol: "http".into(),
                    operation: "extract".into(),
                    scope: "public".into(),
                },
                risk: CapabilityRisk::Medium,
            },
        ]
    }

    async fn execute(
        &self,
        handler: &str,
        input: &serde_json::Value,
        timeout: Duration,
    ) -> crate::Result<serde_json::Value> {
        execute_http(handler, input, timeout)
            .await
            .map_err(|e| crate::SdkError::ExecutionFailed(e))
    }
}

/// Execute HTTP request based on handler type.
async fn execute_http(
    handler: &str,
    input: &serde_json::Value,
    timeout: Duration,
) -> std::result::Result<serde_json::Value, String> {
    match handler {
        "get" => http_get(input, timeout).await,
        "post" => http_post(input, timeout).await,
        "extract" => web_extract(input, timeout).await,
        other => Err(format!("unsupported http handler: {other}")),
    }
}

async fn http_get(
    input: &serde_json::Value,
    timeout: Duration,
) -> std::result::Result<serde_json::Value, String> {
    let url = input
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "http get requires string field `url`".to_string())?;

    let headers: std::collections::HashMap<String, String> = input
        .get("headers")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid http headers: {e}"))?
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    let mut req = client.get(url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let resp_headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?;

    Ok(serde_json::json!({
        "status": status,
        "headers": resp_headers,
        "body": body,
    }))
}

async fn http_post(
    input: &serde_json::Value,
    timeout: Duration,
) -> std::result::Result<serde_json::Value, String> {
    let url = input
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "http post requires string field `url`".to_string())?;

    let body = input
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "http post requires string field `body`".to_string())?;

    let headers: std::collections::HashMap<String, String> = input
        .get("headers")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid http headers: {e}"))?
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    let mut req = client.post(url).body(body.to_string());
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let resp_headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?;

    Ok(serde_json::json!({
        "status": status,
        "headers": resp_headers,
        "body": body,
    }))
}

async fn web_extract(
    input: &serde_json::Value,
    timeout: Duration,
) -> std::result::Result<serde_json::Value, String> {
    let url = input
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "web extract requires string field `url`".to_string())?;

    let headers: std::collections::HashMap<String, String> = input
        .get("headers")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid http headers: {e}"))?
        .unwrap_or_default();

    let response = http_get(
        &serde_json::json!({ "url": url, "headers": headers }),
        timeout,
    )
    .await?;
    let status = response
        .get("status")
        .and_then(|v| v.as_u64())
        .unwrap_or_default();
    let body = response
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing body in fetched response".to_string())?;

    let extracted = extract_html_content(body);

    Ok(serde_json::json!({
        "status": status,
        "url": url,
        "title": extracted.title,
        "description": extracted.description,
        "content_text": extracted.content_text,
        "content_html": extracted.content_html,
        "links": extracted.links,
    }))
}

struct ExtractedContent {
    title: Option<String>,
    description: Option<String>,
    content_text: String,
    content_html: Option<String>,
    links: Vec<String>,
}

fn extract_html_content(html: &str) -> ExtractedContent {
    let document = scraper::Html::parse_document(html);
    let title = select_first_text(&document, "title");
    let description = select_meta_content(&document, "description")
        .or_else(|| select_meta_property(&document, "og:description"));
    let content_html =
        select_first_html(&document, "article").or_else(|| select_first_html(&document, "main"));
    let content_text = content_html
        .as_deref()
        .map(extract_text_fragment)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| extract_text_fragment(html));
    let links = collect_links(&document);

    ExtractedContent {
        title,
        description,
        content_text,
        content_html,
        links,
    }
}

fn select_first_text(document: &scraper::Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .map(|element| normalize_text(&element.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty())
}

fn select_first_html(document: &scraper::Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .map(|element| element.html())
}

fn select_meta_content(document: &scraper::Html, name: &str) -> Option<String> {
    let selector = Selector::parse(&format!(r#"meta[name="{name}"]"#)).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("content"))
        .map(normalize_text)
        .filter(|text| !text.is_empty())
}

fn select_meta_property(document: &scraper::Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!(r#"meta[property="{property}"]"#)).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("content"))
        .map(normalize_text)
        .filter(|text| !text.is_empty())
}

fn collect_links(document: &scraper::Html) -> Vec<String> {
    let Ok(selector) = Selector::parse("a[href]") else {
        return Vec::new();
    };

    document
        .select(&selector)
        .filter_map(|element| element.value().attr("href"))
        .map(normalize_text)
        .filter(|href| !href.is_empty())
        .take(100)
        .collect()
}

fn extract_text_fragment(fragment: &str) -> String {
    let fragment = scraper::Html::parse_fragment(fragment);
    normalize_text(&fragment.root_element().text().collect::<Vec<_>>().join(" "))
}

fn normalize_text(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CapabilityAccess tests ────────────────────────────────────────────────

    #[test]
    fn test_capability_access_eq() {
        assert_eq!(CapabilityAccess::Read, CapabilityAccess::Read);
        assert_eq!(CapabilityAccess::Write, CapabilityAccess::Write);
        assert_eq!(CapabilityAccess::Execute, CapabilityAccess::Execute);
        assert_ne!(CapabilityAccess::Read, CapabilityAccess::Write);
    }

    #[test]
    fn test_capability_access_debug() {
        assert_eq!(format!("{:?}", CapabilityAccess::Read), "Read");
        assert_eq!(format!("{:?}", CapabilityAccess::Write), "Write");
        assert_eq!(format!("{:?}", CapabilityAccess::Execute), "Execute");
    }

    // ── CapabilityRisk tests ───────────────────────────────────────────────────

    #[test]
    fn test_capability_risk_ordering() {
        assert!(CapabilityRisk::Low < CapabilityRisk::Medium);
        assert!(CapabilityRisk::Medium < CapabilityRisk::High);
        assert!(CapabilityRisk::Low <= CapabilityRisk::Low);
        assert!(CapabilityRisk::High >= CapabilityRisk::High);
    }

    #[test]
    fn test_capability_risk_debug() {
        assert_eq!(format!("{:?}", CapabilityRisk::Low), "Low");
        assert_eq!(format!("{:?}", CapabilityRisk::Medium), "Medium");
        assert_eq!(format!("{:?}", CapabilityRisk::High), "High");
    }

    // ── ExecutionCapability tests ──────────────────────────────────────────────

    #[test]
    fn test_execution_capability_network_eq() {
        let cap1 = ExecutionCapability::Network {
            protocol: "http".into(),
            operation: "fetch".into(),
            scope: "public".into(),
        };
        let cap2 = ExecutionCapability::Network {
            protocol: "http".into(),
            operation: "fetch".into(),
            scope: "public".into(),
        };
        let cap3 = ExecutionCapability::Network {
            protocol: "https".into(),
            operation: "fetch".into(),
            scope: "public".into(),
        };
        assert_eq!(cap1, cap2);
        assert_ne!(cap1, cap3);
    }

    #[test]
    fn test_execution_capability_filesystem_eq() {
        let cap1 = ExecutionCapability::Filesystem {
            scope: "/workspace".into(),
            access: CapabilityAccess::Read,
        };
        let cap2 = ExecutionCapability::Filesystem {
            scope: "/workspace".into(),
            access: CapabilityAccess::Read,
        };
        let cap3 = ExecutionCapability::Filesystem {
            scope: "/data".into(),
            access: CapabilityAccess::Read,
        };
        assert_eq!(cap1, cap2);
        assert_ne!(cap1, cap3);
    }

    #[test]
    fn test_execution_capability_tool_eq() {
        let cap1 = ExecutionCapability::Tool {
            name: "bash".into(),
            access: CapabilityAccess::Execute,
            scope: "shell".into(),
        };
        let cap2 = ExecutionCapability::Tool {
            name: "bash".into(),
            access: CapabilityAccess::Execute,
            scope: "shell".into(),
        };
        let cap3 = ExecutionCapability::Tool {
            name: "python".into(),
            access: CapabilityAccess::Execute,
            scope: "shell".into(),
        };
        assert_eq!(cap1, cap2);
        assert_ne!(cap1, cap3);
    }

    #[test]
    fn test_execution_capability_debug() {
        let cap = ExecutionCapability::Network {
            protocol: "http".into(),
            operation: "fetch".into(),
            scope: "public".into(),
        };
        let debug_str = format!("{:?}", cap);
        assert!(debug_str.contains("http"));
        assert!(debug_str.contains("fetch"));
    }

    // ── ExecutionCapabilityGrant tests ────────────────────────────────────────

    #[test]
    fn test_execution_capability_grant_eq() {
        let grant1 = ExecutionCapabilityGrant {
            capability: ExecutionCapability::Network {
                protocol: "http".into(),
                operation: "fetch".into(),
                scope: "public".into(),
            },
            risk: CapabilityRisk::Low,
        };
        let grant2 = ExecutionCapabilityGrant {
            capability: ExecutionCapability::Network {
                protocol: "http".into(),
                operation: "fetch".into(),
                scope: "public".into(),
            },
            risk: CapabilityRisk::Low,
        };
        let grant3 = ExecutionCapabilityGrant {
            capability: ExecutionCapability::Network {
                protocol: "http".into(),
                operation: "fetch".into(),
                scope: "public".into(),
            },
            risk: CapabilityRisk::Medium,
        };
        assert_eq!(grant1, grant2);
        assert_ne!(grant1, grant3);
    }

    // ── HttpExecutionAdapter tests ─────────────────────────────────────────────

    #[test]
    fn test_http_execution_adapter_capabilities() {
        let adapter = HttpExecutionAdapter;
        let caps = adapter.capabilities();
        assert_eq!(caps.len(), 3);

        // Check first capability (http get)
        assert!(matches!(
            &caps[0].capability,
            ExecutionCapability::Network { protocol, operation, scope }
            if protocol == "http" && operation == "fetch" && scope == "public"
        ));
        assert_eq!(caps[0].risk, CapabilityRisk::Low);

        // Check second capability (http post)
        assert!(matches!(
            &caps[1].capability,
            ExecutionCapability::Network { protocol, operation, scope }
            if protocol == "http" && operation == "post" && scope == "public"
        ));
        assert_eq!(caps[1].risk, CapabilityRisk::Medium);

        // Check third capability (web extract)
        assert!(matches!(
            &caps[2].capability,
            ExecutionCapability::Network { protocol, operation, scope }
            if protocol == "http" && operation == "extract" && scope == "public"
        ));
        assert_eq!(caps[2].risk, CapabilityRisk::Medium);
    }

    // ── HTML extraction helper tests ─────────────────────────────────────────

    #[test]
    fn test_extract_html_content_with_title() {
        let html = r#"<!DOCTYPE html>
<html>
<head><title>Test Page Title</title></head>
<body><main><p>Hello World</p></main></body>
</html>"#;
        let result = extract_html_content(html);
        assert_eq!(result.title, Some("Test Page Title".to_string()));
        // main element isolates content, so only body text is extracted
        assert_eq!(result.content_text, "Hello World");
    }

    #[test]
    fn test_extract_html_content_with_description() {
        let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta name="description" content="Page description text">
</head>
<body><p>Content here</p></body>
</html>"#;
        let result = extract_html_content(html);
        assert_eq!(
            result.description,
            Some("Page description text".to_string())
        );
    }

    #[test]
    fn test_extract_html_content_with_og_description() {
        let html = r#"<!DOCTYPE html>
<html>
<head>
    <meta property="og:description" content="OpenGraph description">
</head>
<body><p>Content</p></body>
</html>"#;
        let result = extract_html_content(html);
        assert_eq!(
            result.description,
            Some("OpenGraph description".to_string())
        );
    }

    #[test]
    fn test_extract_html_content_with_article() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<article><p>Article paragraph one</p><p>Article paragraph two</p></article>
<footer>Footer content</footer>
</body>
</html>"#;
        let result = extract_html_content(html);
        assert!(result.content_html.is_some());
        assert!(result.content_text.contains("Article paragraph"));
        assert!(!result.content_text.contains("Footer"));
    }

    #[test]
    fn test_extract_html_content_with_main() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<main><p>Main content</p></main>
<aside><p>Sidebar content</p></aside>
</body>
</html>"#;
        let result = extract_html_content(html);
        assert!(result.content_html.is_some());
        assert!(result.content_text.contains("Main content"));
        assert!(!result.content_text.contains("Sidebar"));
    }

    #[test]
    fn test_extract_html_content_empty_html() {
        let html = "";
        let result = extract_html_content(html);
        assert_eq!(result.title, None);
        assert_eq!(result.description, None);
        // normalize_text on empty gives empty, so content_text should be empty
        assert!(result.content_text.is_empty());
    }

    #[test]
    fn test_extract_html_content_with_links() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<a href="https://example.com">Link 1</a>
<a href="https://test.com">Link 2</a>
<a href="https://foo.com">Link 3</a>
</body>
</html>"#;
        let result = extract_html_content(html);
        assert_eq!(result.links.len(), 3);
        assert!(result.links.contains(&"https://example.com".to_string()));
        assert!(result.links.contains(&"https://test.com".to_string()));
        assert!(result.links.contains(&"https://foo.com".to_string()));
    }

    #[test]
    fn test_extract_html_content_links_limit() {
        let mut html = String::from("<html><body>");
        for i in 0..=150 {
            html.push_str(&format!(
                r#"<a href="https://example{}.com">Link {}</a>"#,
                i, i
            ));
        }
        html.push_str("</body></html>");
        let result = extract_html_content(&html);
        // Should be limited to 100 links
        assert!(result.links.len() <= 100);
    }

    #[test]
    fn test_extract_html_content_ignores_empty_hrefs() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<a href="https://valid.com">Valid</a>
<a href="">Empty href</a>
<a>No href</a>
</body>
</html>"#;
        let result = extract_html_content(html);
        assert!(result.links.contains(&"https://valid.com".to_string()));
        assert!(!result.links.contains(&"Empty href".to_string()));
    }

    // ── normalize_text tests ─────────────────────────────────────────────────

    #[test]
    fn test_normalize_text_simple() {
        assert_eq!(normalize_text("hello world"), "hello world");
    }

    #[test]
    fn test_normalize_text_multiple_spaces() {
        assert_eq!(normalize_text("hello    world"), "hello world");
    }

    #[test]
    fn test_normalize_text_tabs_newlines() {
        assert_eq!(normalize_text("hello\t\nworld"), "hello world");
    }

    #[test]
    fn test_normalize_text_empty() {
        assert_eq!(normalize_text(""), "");
    }

    #[test]
    fn test_normalize_text_only_whitespace() {
        assert_eq!(normalize_text("   \t\n  "), "");
    }

    // ── select_first_text tests ──────────────────────────────────────────────

    #[test]
    fn test_select_first_text_found() {
        let html = r#"<html><head><title>Found Title</title></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_first_text(&document, "title");
        assert_eq!(result, Some("Found Title".to_string()));
    }

    #[test]
    fn test_select_first_text_not_found() {
        let html = r#"<html><head></head><body><p>Content</p></body></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_first_text(&document, "title");
        assert_eq!(result, None);
    }

    #[test]
    fn test_select_first_text_empty_content() {
        let html = r#"<html><head><title></title></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_first_text(&document, "title");
        assert_eq!(result, None);
    }

    // ── select_first_html tests ──────────────────────────────────────────────

    #[test]
    fn test_select_first_html_found() {
        let html = r#"<html><body><article><p>Test</p></article></body></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_first_html(&document, "article");
        assert!(result.is_some());
        assert!(result.unwrap().contains("<p>"));
    }

    #[test]
    fn test_select_first_html_not_found() {
        let html = r#"<html><body><p>Content</p></body></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_first_html(&document, "article");
        assert_eq!(result, None);
    }

    // ── select_meta_content tests ────────────────────────────────────────────

    #[test]
    fn test_select_meta_content_found() {
        let html =
            r#"<html><head><meta name="description" content="Test Description"></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_meta_content(&document, "description");
        assert_eq!(result, Some("Test Description".to_string()));
    }

    #[test]
    fn test_select_meta_content_not_found() {
        let html = r#"<html><head><meta name="keywords" content="test"></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_meta_content(&document, "description");
        assert_eq!(result, None);
    }

    // ── select_meta_property tests ───────────────────────────────────────────

    #[test]
    fn test_select_meta_property_found() {
        let html = r#"<html><head><meta property="og:title" content="OG Title"></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_meta_property(&document, "og:title");
        assert_eq!(result, Some("OG Title".to_string()));
    }

    #[test]
    fn test_select_meta_property_not_found() {
        let html = r#"<html><head><meta property="og:type" content="website"></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_meta_property(&document, "og:title");
        assert_eq!(result, None);
    }

    // ── extract_text_fragment tests ──────────────────────────────────────────

    #[test]
    fn test_extract_text_fragment_simple() {
        let html = "<p>Hello World</p>";
        let result = extract_text_fragment(html);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_extract_text_fragment_with_nested() {
        let html = "<div><span><strong>Nested</strong> text</span></div>";
        let result = extract_text_fragment(html);
        assert_eq!(result, "Nested text");
    }

    // ── collect_links tests ──────────────────────────────────────────────────

    #[test]
    fn test_collect_links_with_hrefs() {
        let html = r#"<html><body><a href="https://example.com">Link</a></body></html>"#;
        let document = scraper::Html::parse_document(html);
        let links = collect_links(&document);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "https://example.com");
    }

    #[test]
    fn test_collect_links_empty_html() {
        let html = "<html><body></body></html>";
        let document = scraper::Html::parse_document(html);
        let links = collect_links(&document);
        assert!(links.is_empty());
    }

    #[test]
    fn test_collect_links_invalid_html() {
        let html = "not valid html at all";
        let document = scraper::Html::parse_document(html);
        let links = collect_links(&document);
        assert!(links.is_empty());
    }

    // ── execute_http error handling tests ───────────────────────────────────

    #[tokio::test]
    async fn test_execute_http_unsupported_handler() {
        let result = execute_http(
            "unsupported",
            &serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported http handler"));
    }

    #[tokio::test]
    async fn test_execute_http_get_missing_url() {
        let result = execute_http("get", &serde_json::json!({}), Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("http get requires string field `url`"));
    }

    #[tokio::test]
    async fn test_execute_http_post_missing_url() {
        let result = execute_http("post", &serde_json::json!({}), Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("http post requires string field `url`"));
    }

    #[tokio::test]
    async fn test_execute_http_post_missing_body() {
        let result = execute_http(
            "post",
            &serde_json::json!({ "url": "https://example.com" }),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("http post requires string field `body`"));
    }

    #[tokio::test]
    async fn test_execute_http_extract_missing_url() {
        let result = execute_http("extract", &serde_json::json!({}), Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("web extract requires string field `url`"));
    }

    #[tokio::test]
    async fn test_execute_http_get_invalid_headers() {
        let result = execute_http(
            "get",
            &serde_json::json!({
                "url": "https://example.com",
                "headers": "not a map"
            }),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid http headers"));
    }

    // ── HttpExecutionAdapter execute tests ───────────────────────────────────

    #[tokio::test]
    async fn test_http_execution_adapter_execute_unsupported() {
        let adapter = HttpExecutionAdapter;
        let result = adapter
            .execute(
                "unsupported",
                &serde_json::json!({}),
                Duration::from_secs(5),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_execution_adapter_execute_get_no_url() {
        let adapter = HttpExecutionAdapter;
        let result = adapter
            .execute("get", &serde_json::json!({}), Duration::from_secs(5))
            .await;
        assert!(result.is_err());
    }

    // ── Additional edge case tests ─────────────────────────────────────────────

    #[test]
    fn test_extract_text_fragment_with_whitespace() {
        // Test that whitespace is properly normalized
        let html = "<div>\n  \t  Hello   World  \n</div>";
        let result = extract_text_fragment(html);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_extract_text_fragment_empty_div() {
        let html = "<div></div>";
        let result = extract_text_fragment(html);
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_text_fragment_with_scripts() {
        // Script content should be included in text (scraper doesn't execute JS)
        let html = "<html><body><script>alert('x')</script><p>Visible</p></body></html>";
        let result = extract_text_fragment(html);
        assert!(result.contains("Visible"));
        assert!(result.contains("alert('x')"));
    }

    #[test]
    fn test_normalize_text_unicode() {
        // Test with unicode characters
        assert_eq!(normalize_text("Hello   World"), "Hello World");
        assert_eq!(normalize_text("日本語"), "日本語");
    }

    #[test]
    fn test_normalize_text_newlines() {
        assert_eq!(normalize_text("line1\nline2"), "line1 line2");
        assert_eq!(normalize_text("a\tb"), "a b");
        assert_eq!(normalize_text("a\n\n\nb"), "a b");
    }

    #[test]
    fn test_select_first_text_with_whitespace_selector() {
        let html = r#"<html><body><p>Content</p></body></html>"#;
        let document = scraper::Html::parse_document(html);
        // Empty or whitespace selector should return None
        let result = select_first_text(&document, " ");
        assert_eq!(result, None);
    }

    #[test]
    fn test_select_meta_content_with_empty_content() {
        let html = r#"<html><head><meta name="description" content=""></head></html>"#;
        let document = scraper::Html::parse_document(html);
        let result = select_meta_content(&document, "description");
        // Empty content should be filtered out
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_html_content_with_only_whitespace() {
        let html = "   \n\t  ";
        let result = extract_html_content(html);
        assert_eq!(result.title, None);
        assert_eq!(result.description, None);
        assert!(result.content_text.is_empty());
    }

    #[test]
    fn test_extract_html_content_with_nested_main() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<main><div><p>Nested paragraph</p></div></main>
</body>
</html>"#;
        let result = extract_html_content(html);
        assert!(result.content_text.contains("Nested paragraph"));
    }

    #[test]
    fn test_extract_html_content_without_article_or_main() {
        let html = r#"<!DOCTYPE html>
<html>
<body>
<div><p>Just a div</p></div>
</body>
</html>"#;
        let result = extract_html_content(html);
        // Falls back to full HTML text extraction
        assert!(result.content_text.contains("Just a div"));
    }

    #[test]
    fn test_collect_links_deduplicates() {
        let html = r#"<html>
<body>
<a href="https://example.com">Link 1</a>
<a href="https://example.com">Link 2</a>
</body>
</html>"#;
        let document = scraper::Html::parse_document(html);
        let links = collect_links(&document);
        // Should contain duplicates
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_collect_links_with_whitespace_href() {
        let html = r#"<html><body><a href="  https://example.com  ">Link</a></body></html>"#;
        let document = scraper::Html::parse_document(html);
        let links = collect_links(&document);
        assert_eq!(links.len(), 1);
        // Whitespace should be normalized
        assert!(links[0].contains("https://example.com"));
    }
}
