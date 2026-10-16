use std::fmt::Write;
use std::time::Duration;

use bytes::Bytes;
use http::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use http::{HeaderValue, Response, StatusCode};
use http_body_util::Full;

/// A page the proxy answers with instead of forwarding, rendered as HTML for
/// browsers and plain text otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorPage {
    pub status: StatusCode,
    pub title: String,
    pub detail: String,
    /// Recent output of the instance, shown verbatim.
    pub log: Vec<String>,
    /// Sent as `Retry-After`, e.g. for a 503 after a boot timeout.
    pub retry_after: Option<Duration>,
}

impl ErrorPage {
    pub fn new(status: StatusCode, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            title: title.into(),
            detail: detail.into(),
            log: Vec::new(),
            retry_after: None,
        }
    }

    pub fn with_log(mut self, log: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.log = log.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Renders the page for a request with the given headers.
    pub fn render(&self, request_headers: &HeaderMap) -> Response<Full<Bytes>> {
        let (content_type, body) = if wants_html(request_headers) {
            ("text/html; charset=utf-8", self.html())
        } else {
            ("text/plain; charset=utf-8", self.text())
        };
        let mut response = Response::new(Full::new(Bytes::from(body)));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if let Some(retry_after) = self.retry_after {
            headers.insert(RETRY_AFTER, retry_after.as_secs().max(1).into());
        }
        response
    }

    fn text(&self) -> String {
        let mut text = format!(
            "{} {}\n\n{}\n",
            self.status.as_u16(),
            self.title,
            self.detail
        );
        if !self.log.is_empty() {
            text.push('\n');
            for line in &self.log {
                text.push_str(line);
                text.push('\n');
            }
        }
        text
    }

    fn html(&self) -> String {
        let title = escape(&self.title);
        let mut html = format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; --fg: #1d1d1f; --muted: #6e6e73; --bg: #fbfbfd; --panel: #f0f0f3; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg: #f5f5f7; --muted: #a1a1a6; --bg: #161618; --panel: #232326; }} }}
body {{ margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
main {{ max-width: 52rem; margin: 0 auto; padding: 4rem 1rem; }}
.status {{ color: var(--muted); font-size: .875rem; letter-spacing: .05em; text-transform: uppercase; margin: 0; }}
h1 {{ font-size: 1.75rem; line-height: 1.2; margin: .25rem 0 1rem; }}
p {{ margin: 0 0 1.5rem; white-space: pre-wrap; overflow-wrap: anywhere; }}
pre {{ background: var(--panel); border-radius: .5rem; padding: 1rem; overflow-x: auto; font: 13px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }}
</style>
</head>
<body>
<main>
<p class="status">{status}</p>
<h1>{title}</h1>
<p>{detail}</p>
"#,
            status = self.status.as_u16(),
            detail = escape(&self.detail),
        );
        if !self.log.is_empty() {
            html.push_str("<pre>");
            for line in &self.log {
                let _ = writeln!(html, "{}", escape(line));
            }
            html.push_str("</pre>\n");
        }
        html.push_str("</main>\n</body>\n</html>\n");
        html
    }
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("text/html"))
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    fn page() -> ErrorPage {
        ErrorPage::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "<frontend> didn't start",
            "Waited 60s for \"web\" & gave up",
        )
        .with_log(["Booting <puma>", "Listening on 'tcp://127.0.0.1'"])
    }

    async fn render(page: &ErrorPage, accept: Option<&'static str>) -> (Response<()>, String) {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(ACCEPT, HeaderValue::from_static(accept));
        }
        let (parts, body) = page.render(&headers).into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (
            Response::from_parts(parts, ()),
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    #[tokio::test]
    async fn renders_escaped_html_for_browsers() {
        let (response, body) = render(
            &page(),
            Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(body.contains("<title>&lt;frontend&gt; didn&#39;t start</title>"));
        assert!(body.contains("<h1>&lt;frontend&gt; didn&#39;t start</h1>"));
        assert!(body.contains("Waited 60s for &quot;web&quot; &amp; gave up"));
        assert!(
            body.contains(
                "<pre>Booting &lt;puma&gt;\nListening on &#39;tcp://127.0.0.1&#39;\n</pre>"
            )
        );
        assert!(!body.contains("<frontend>"));
        assert!(!body.contains("<puma>"));
    }

    #[tokio::test]
    async fn renders_plain_text_otherwise() {
        for accept in [None, Some("*/*"), Some("application/json")] {
            let (response, body) = render(&page(), accept).await;
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "text/plain; charset=utf-8"
            );
            assert_eq!(
                body,
                "503 <frontend> didn't start\n\nWaited 60s for \"web\" & gave up\n\n\
                 Booting <puma>\nListening on 'tcp://127.0.0.1'\n"
            );
        }
    }

    #[tokio::test]
    async fn omits_the_log_when_empty() {
        let page = ErrorPage::new(StatusCode::NOT_FOUND, "Not found", "nothing here");
        let (_, html) = render(&page, Some("text/html")).await;
        assert!(!html.contains("<pre>"));
        let (_, text) = render(&page, None).await;
        assert_eq!(text, "404 Not found\n\nnothing here\n");
    }

    #[tokio::test]
    async fn sets_retry_after_only_when_given() {
        let (response, _) = render(&page(), None).await;
        assert!(response.headers().get(RETRY_AFTER).is_none());

        let page = page().with_retry_after(Duration::from_secs(5));
        let (response, _) = render(&page, None).await;
        assert_eq!(response.headers()[RETRY_AFTER], "5");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    }
}
