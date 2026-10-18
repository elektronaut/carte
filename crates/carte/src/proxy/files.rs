//! Files served from an app's `serve_files` folder, ahead of the app.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header};
use http_body_util::{Empty, StreamBody};
use hyper::body::Frame;
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

use super::ActivityGuard;
use super::body::{GuardedBody, ProxyBody, boxed};

/// The file in `root` a request path names, looked up like Rails'
/// `ActionDispatch::Static`: `/about` tries `about`, `about.html` and
/// `about/index.html`. Dotfiles (other than `.well-known`) and anything that
/// resolves outside `root` are never found.
pub fn find(root: &Path, path: &str) -> Option<PathBuf> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    let relative = decoded.trim_start_matches('/');
    let mut clean = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part.starts_with('.') && part != ".well-known" {
                    return None;
                }
                clean.push(part);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    let base = root.join(&clean);
    let mut candidates = Vec::new();
    if !relative.is_empty() && !relative.ends_with('/') {
        candidates.push(base.clone());
        let mut html = base.clone().into_os_string();
        html.push(".html");
        candidates.push(PathBuf::from(html));
    }
    candidates.push(base.join("index.html"));
    let root = root.canonicalize().ok()?;
    candidates.into_iter().find_map(|candidate| {
        let real = candidate.canonicalize().ok()?;
        (real.starts_with(&root) && real.is_file()).then_some(real)
    })
}

/// Answers a GET or HEAD with the file, or 304 when the client's copy is current.
pub(crate) async fn respond(
    path: &Path,
    method: &Method,
    headers: &HeaderMap,
    guard: Box<dyn ActivityGuard>,
) -> Response<ProxyBody> {
    let (file, metadata) = match tokio::fs::File::open(path).await {
        Ok(file) => match file.metadata().await {
            Ok(metadata) => (file, metadata),
            Err(_) => return status(StatusCode::NOT_FOUND),
        },
        Err(_) => return status(StatusCode::NOT_FOUND),
    };
    let modified = metadata.modified().ok().map(truncate_to_seconds);
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CACHE_CONTROL, "no-cache");
    if let Some(modified) = modified {
        response = response.header(header::LAST_MODIFIED, httpdate::fmt_http_date(modified));
        let since = headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok());
        if since.is_some_and(|since| modified <= since) {
            return response
                .status(StatusCode::NOT_MODIFIED)
                .body(boxed(Empty::new()))
                .expect("valid response");
        }
    }
    let response = response.header(header::CONTENT_LENGTH, metadata.len());
    if method == Method::HEAD {
        return response.body(boxed(Empty::new())).expect("valid response");
    }
    let stream = ReaderStream::new(file).map(|chunk| chunk.map(Frame::<Bytes>::data));
    response
        .body(boxed(GuardedBody::new(StreamBody::new(stream), guard)))
        .expect("valid response")
}

fn status(code: StatusCode) -> Response<ProxyBody> {
    Response::builder()
        .status(code)
        .body(boxed(Empty::new()))
        .expect("valid response")
}

fn truncate_to_seconds(time: SystemTime) -> SystemTime {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
}

fn content_type(path: &Path) -> HeaderValue {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    HeaderValue::from_static(match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "xml" => "application/xml",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, text) in [
            ("index.html", "home"),
            ("about.html", "about"),
            ("blog/index.html", "blog"),
            ("css/site.css", "body {}"),
            ("hello world.txt", "hi"),
            (".env", "SECRET=1"),
            (".well-known/security.txt", "contact"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        dir
    }

    fn found(root: &Path, path: &str) -> Option<String> {
        let root = root.canonicalize().unwrap();
        find(&root, path).map(|p| {
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
    }

    #[test]
    fn looks_up_files_like_rails() {
        let site = site();
        let root = site.path();
        for (path, file) in [
            ("/", Some("index.html")),
            ("/?utm=1", Some("index.html")),
            ("/about", Some("about.html")),
            ("/about.html", Some("about.html")),
            ("/blog", Some("blog/index.html")),
            ("/blog/", Some("blog/index.html")),
            ("/css/site.css", Some("css/site.css")),
            ("/hello%20world.txt", Some("hello world.txt")),
            (
                "/.well-known/security.txt",
                Some(".well-known/security.txt"),
            ),
            ("/missing", None),
            ("/about/", None),
            ("/css", None),
        ] {
            assert_eq!(found(root, path).as_deref(), file, "{path}");
        }
    }

    #[test]
    fn never_leaves_the_folder_or_shows_dotfiles() {
        let site = site();
        let outside = site.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let public = site.path().join("public");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("index.html"), "public").unwrap();
        std::os::unix::fs::symlink(&outside, public.join("link.txt")).unwrap();
        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/..%2fsecret.txt",
            "/link.txt",
            "/.env",
            "/%2eenv",
            "/a%00b",
        ] {
            assert_eq!(found(&public, path), None, "{path}");
        }
        assert_eq!(found(site.path(), "/.env"), None);
    }
}
