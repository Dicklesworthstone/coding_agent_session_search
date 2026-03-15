//! Local preview server for Pages exports.
//!
//! Provides a local HTTP server to preview exported archives before deployment.
//! Features:
//! - Static file serving with correct MIME types
//! - COOP/COEP headers for full WebCrypto functionality
//! - Auto-open browser on start
//! - Graceful shutdown via Ctrl+C

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tracing::debug;

/// Error type for preview server operations.
#[derive(Debug)]
pub enum PreviewError {
    /// Failed to bind to the specified port.
    BindFailed { port: u16, source: std::io::Error },
    /// The site directory does not exist.
    SiteDirectoryNotFound(PathBuf),
    /// Failed to read a file.
    FileReadError {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Failed to open browser.
    BrowserOpenFailed(String),
    /// Server error.
    ServerError(String),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindFailed { port, source } => {
                write!(f, "Failed to bind to port {}: {}", port, source)
            }
            Self::SiteDirectoryNotFound(path) => {
                write!(f, "Site directory not found: {}", path.display())
            }
            Self::FileReadError { path, source } => {
                write!(f, "Failed to read file {}: {}", path.display(), source)
            }
            Self::BrowserOpenFailed(msg) => {
                write!(f, "Failed to open browser: {}", msg)
            }
            Self::ServerError(msg) => {
                write!(f, "Server error: {}", msg)
            }
        }
    }
}

impl std::error::Error for PreviewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BindFailed { source, .. } => Some(source),
            Self::FileReadError { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Configuration for the preview server.
#[derive(Debug, Clone)]
pub struct PreviewConfig {
    /// Directory containing the site to serve.
    pub site_dir: PathBuf,
    /// Port to listen on.
    pub port: u16,
    /// Whether to automatically open a browser.
    pub open_browser: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            site_dir: PathBuf::from("."),
            port: 8080,
            open_browser: true,
        }
    }
}

/// Guess MIME type from file extension.
fn guess_mime_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("bin") => "application/octet-stream",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        _ => "application/octet-stream",
    }
}

/// Build an HTTP response with the given status code, content type, and body.
fn build_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    build_response_with_content_length(status, content_type, body, None)
}

/// Build an HTTP response with an optional explicit Content-Length override.
fn build_response_with_content_length(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    content_length_override: Option<usize>,
) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let content_length = content_length_override.unwrap_or(body.len());

    let headers = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Cross-Origin-Embedder-Policy: require-corp\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n",
        status, status_text, content_type, content_length
    );

    let mut response = headers.into_bytes();
    response.extend(body);
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadLengthSource {
    Metadata,
    FallbackRead,
}

/// Resolve representation length for HEAD requests without eagerly reading file bytes.
///
/// Falls back to reading the file if metadata is unavailable or does not fit usize.
fn head_content_length_with_metadata_hint(
    file_path: &std::path::Path,
    metadata_length: std::io::Result<u64>,
) -> std::io::Result<(usize, HeadLengthSource)> {
    match metadata_length {
        Ok(metadata_length) => match usize::try_from(metadata_length) {
            Ok(length) => Ok((length, HeadLengthSource::Metadata)),
            Err(_) => {
                let bytes = std::fs::read(file_path)?;
                Ok((bytes.len(), HeadLengthSource::FallbackRead))
            }
        },
        Err(_) => {
            let bytes = std::fs::read(file_path)?;
            Ok((bytes.len(), HeadLengthSource::FallbackRead))
        }
    }
}

fn head_content_length(file_path: &std::path::Path) -> std::io::Result<(usize, HeadLengthSource)> {
    let metadata_length = std::fs::metadata(file_path).map(|meta| meta.len());
    head_content_length_with_metadata_hint(file_path, metadata_length)
}

fn head_content_length_from_hint_or_fs(
    file_path: &std::path::Path,
    metadata_length_hint: Option<u64>,
) -> std::io::Result<(usize, HeadLengthSource)> {
    match metadata_length_hint {
        Some(metadata_length) => {
            head_content_length_with_metadata_hint(file_path, Ok(metadata_length))
        }
        None => head_content_length(file_path),
    }
}

/// Handle a single HTTP request against an already-canonicalized site root.
fn handle_request_with_site_root(site_root_canonical: &std::path::Path, request: &str) -> Vec<u8> {
    // Parse the request line
    let request_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = request_line.split_whitespace().collect();

    if parts.len() < 2 {
        return build_response(400, "text/plain", b"Bad Request".to_vec());
    }

    let method = parts[0];
    let raw_path = parts[1];

    // Only support GET and HEAD
    if method != "GET" && method != "HEAD" {
        return build_response(405, "text/plain", b"Method Not Allowed".to_vec());
    }

    // Strip query/fragment, then decode URL and sanitize path
    let path_only = raw_path
        .split('?')
        .next()
        .unwrap_or(raw_path)
        .split('#')
        .next()
        .unwrap_or(raw_path);
    let decoded_path = urlencoding::decode(path_only).unwrap_or_else(|_| path_only.into());
    let request_path = decoded_path.trim_start_matches('/');

    // Prevent directory traversal
    if request_path.contains("..") {
        return build_response(400, "text/plain", b"Invalid Path".to_vec());
    }

    // Determine the file path
    let file_path = if request_path.is_empty() || request_path == "/" {
        site_root_canonical.join("index.html")
    } else {
        site_root_canonical.join(request_path)
    };

    // Canonicalize to prevent path traversal
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // Try with index.html if it's a directory
            let with_index = file_path.join("index.html");
            match with_index.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    return build_response(404, "text/plain", b"Not Found".to_vec());
                }
            }
        }
    };

    // Ensure the path is within the site directory
    if !canonical.starts_with(site_root_canonical) {
        return build_response(400, "text/plain", b"Invalid Path".to_vec());
    }

    // Check if it's a directory and append index.html if so
    let mut file_to_read = canonical.clone();
    let mut metadata_length_hint = None;
    if let Ok(meta) = std::fs::metadata(&canonical) {
        if meta.is_dir() {
            file_to_read = canonical.join("index.html");
        } else {
            metadata_length_hint = Some(meta.len());
        }
    }

    let request_started = Instant::now();

    if method == "HEAD" {
        match head_content_length_from_hint_or_fs(&file_to_read, metadata_length_hint) {
            Ok((content_length, length_source)) => {
                let mime = guess_mime_type(&file_to_read);
                debug!(
                    method = method,
                    request_path = %request_path,
                    file_path = %file_to_read.display(),
                    status = 200,
                    size_source = ?length_source,
                    content_length = content_length,
                    elapsed_ms = request_started.elapsed().as_millis(),
                    "Preview served HEAD request"
                );
                build_response_with_content_length(200, mime, Vec::new(), Some(content_length))
            }
            Err(err) => {
                debug!(
                    method = method,
                    request_path = %request_path,
                    file_path = %file_to_read.display(),
                    status = 404,
                    error = %err,
                    elapsed_ms = request_started.elapsed().as_millis(),
                    "Preview HEAD request failed"
                );
                build_response(404, "text/plain", b"Not Found".to_vec())
            }
        }
    } else {
        match std::fs::read(&file_to_read) {
            Ok(contents) => {
                let content_length = contents.len();
                let mime = guess_mime_type(&file_to_read);
                debug!(
                    method = method,
                    request_path = %request_path,
                    file_path = %file_to_read.display(),
                    status = 200,
                    size_source = "body_read",
                    content_length = content_length,
                    elapsed_ms = request_started.elapsed().as_millis(),
                    "Preview served GET request"
                );
                build_response(200, mime, contents)
            }
            Err(err) => {
                debug!(
                    method = method,
                    request_path = %request_path,
                    file_path = %file_to_read.display(),
                    status = 404,
                    error = %err,
                    elapsed_ms = request_started.elapsed().as_millis(),
                    "Preview GET request failed"
                );
                build_response(404, "text/plain", b"Not Found".to_vec())
            }
        }
    }
}

/// Handle a single HTTP request.
///
/// This wrapper canonicalizes the provided site directory once per call and then
/// delegates to the canonical-root hot path handler.
#[cfg(test)]
fn handle_request(site_dir: &std::path::Path, request: &str) -> Vec<u8> {
    let site_root_canonical = match site_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return build_response(500, "text/plain", b"Internal Server Error".to_vec());
        }
    };
    handle_request_with_site_root(&site_root_canonical, request)
}

/// Handle a single TCP connection using blocking I/O.
fn handle_connection(mut stream: std::net::TcpStream, site_dir: &std::path::Path) {
    use std::io::{Read, Write};

    // Set a reasonable read timeout so slow clients don't block the thread indefinitely
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    let mut buf = vec![0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request = String::from_utf8_lossy(&buf[..n]);
    let response = handle_request_with_site_root(site_dir, &request);

    let _ = stream.write_all(&response);
    let _ = stream.flush();

    // Explicitly shutdown the connection to clean up resources promptly
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Start the preview server.
///
/// This function will block until the server is shut down (via Ctrl+C).
///
/// # Arguments
///
/// * `config` - Server configuration
///
/// # Returns
///
/// Returns `Ok(())` on graceful shutdown, or an error if the server fails to start.
pub async fn start_preview_server(config: PreviewConfig) -> Result<(), PreviewError> {
    // Verify site directory exists
    if !config.site_dir.exists() {
        return Err(PreviewError::SiteDirectoryNotFound(config.site_dir));
    }

    let site_dir = Arc::new(
        config
            .site_dir
            .canonicalize()
            .map_err(|_| PreviewError::SiteDirectoryNotFound(config.site_dir.clone()))?,
    );

    // Bind to the address (synchronous — preview is a simple dev server)
    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    let listener = std::net::TcpListener::bind(addr).map_err(|e| PreviewError::BindFailed {
        port: config.port,
        source: e,
    })?;

    // Print startup message
    eprintln!();
    eprintln!(
        "\x1b[1;32m\u{1F310}\x1b[0m Preview server running at \x1b[1;36mhttp://localhost:{}\x1b[0m",
        config.port
    );
    eprintln!("   Serving: \x1b[33m{}\x1b[0m", site_dir.display());
    eprintln!("   Press \x1b[1mCtrl+C\x1b[0m to stop");
    eprintln!();

    // Open browser if requested
    if config.open_browser {
        let url = format!("http://localhost:{}", config.port);
        if let Err(e) = open_browser(&url) {
            eprintln!("\x1b[33mWarning:\x1b[0m Could not open browser: {}", e);
            eprintln!("   Please open \x1b[1;36m{}\x1b[0m manually", url);
        }
    }

    // Run blocking accept loop on the blocking pool. The process terminates
    // on SIGINT via the default handler, which is fine for a dev preview server.
    asupersync::runtime::spawn_blocking(move || {
        for stream_result in listener.incoming() {
            match stream_result {
                Ok(stream) => {
                    let site_dir = Arc::clone(&site_dir);
                    std::thread::spawn(move || {
                        handle_connection(stream, &site_dir);
                    });
                }
                Err(e) => {
                    eprintln!("Accept error: {}", e);
                }
            }
        }
    })
    .await;

    eprintln!("\x1b[32mPreview server stopped.\x1b[0m");
    Ok(())
}

/// Open the default browser to the given URL.
fn open_browser(url: &str) -> Result<(), PreviewError> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| PreviewError::BrowserOpenFailed(e.to_string()))?;
    }

    #[cfg(target_os = "linux")]
    {
        // Try xdg-open first, fall back to common browsers
        let browsers = [
            "xdg-open",
            "firefox",
            "chromium",
            "google-chrome",
            "x-www-browser",
        ];
        let mut opened = false;

        for browser in browsers {
            if std::process::Command::new(browser).arg(url).spawn().is_ok() {
                opened = true;
                break;
            }
        }

        if !opened {
            return Err(PreviewError::BrowserOpenFailed(
                "No browser found. Install xdg-open or a web browser.".to_string(),
            ));
        }
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map_err(|e| PreviewError::BrowserOpenFailed(e.to_string()))?;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        return Err(PreviewError::BrowserOpenFailed(
            "Unsupported platform for auto-open".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn content_length(resp: &str) -> Option<usize> {
        resp.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("Content-Length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
    }

    #[test]
    fn test_guess_mime_type() {
        assert_eq!(
            guess_mime_type(std::path::Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("app.js")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("styles.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("data.json")),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("module.wasm")),
            "application/wasm"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("image.png")),
            "image/png"
        );
        assert_eq!(
            guess_mime_type(std::path::Path::new("unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_preview_config_default() {
        let config = PreviewConfig::default();
        assert_eq!(config.port, 8080);
        assert!(config.open_browser);
    }

    #[test]
    fn test_build_response_headers() {
        let response = build_response(200, "text/html", b"<html></html>".to_vec());
        let response_str = String::from_utf8_lossy(&response);

        assert!(response_str.contains("HTTP/1.1 200 OK"));
        assert!(response_str.contains("Content-Type: text/html"));
        assert!(response_str.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(response_str.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert!(response_str.contains("Cross-Origin-Resource-Policy: same-origin"));
    }

    #[test]
    fn test_handle_request_bad_method() {
        let site_dir = std::path::Path::new("/tmp");
        let response = handle_request(site_dir, "POST / HTTP/1.1\r\n");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("HTTP/1.1 405 Method Not Allowed"));
        assert!(response_str.contains("Method Not Allowed"));
    }

    #[test]
    fn test_handle_request_bad_path() {
        let site_dir = std::path::Path::new("/tmp");
        let response = handle_request(site_dir, "GET /../etc/passwd HTTP/1.1\r\n");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("400") || response_str.contains("Invalid"));
    }

    #[test]
    fn test_handle_request_serves_index_with_coi_headers() {
        let temp_dir = TempDir::new().expect("temp dir");
        let site_dir = temp_dir.path();

        std::fs::write(
            site_dir.join("index.html"),
            "<!doctype html><html>ok</html>",
        )
        .expect("write index.html");
        std::fs::write(
            site_dir.join("sw.js"),
            "self.addEventListener('install', () => {});",
        )
        .expect("write sw.js");

        let index_response = handle_request(site_dir, "GET / HTTP/1.1\r\n");
        let index_str = String::from_utf8_lossy(&index_response);

        assert!(index_str.contains("HTTP/1.1 200 OK"));
        assert!(index_str.contains("Content-Type: text/html; charset=utf-8"));
        assert!(index_str.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(index_str.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert!(index_str.contains("Cross-Origin-Resource-Policy: same-origin"));

        let sw_response = handle_request(site_dir, "GET /sw.js HTTP/1.1\r\n");
        let sw_str = String::from_utf8_lossy(&sw_response);
        assert!(sw_str.contains("HTTP/1.1 200 OK"));
        assert!(sw_str.contains("Content-Type: application/javascript; charset=utf-8"));
    }

    #[test]
    fn test_handle_request_head_preserves_content_length() {
        let temp_dir = TempDir::new().expect("temp dir");
        let site_dir = temp_dir.path();
        let body = "<!doctype html><html>head-check</html>";
        std::fs::write(site_dir.join("index.html"), body).expect("write index");

        let get_response = handle_request(site_dir, "GET / HTTP/1.1\r\n");
        let head_response = handle_request(site_dir, "HEAD / HTTP/1.1\r\n");

        let get_str = String::from_utf8_lossy(&get_response);
        let head_str = String::from_utf8_lossy(&head_response);

        let get_len = content_length(&get_str).expect("GET content-length");
        let head_len = content_length(&head_str).expect("HEAD content-length");
        assert_eq!(head_len, get_len);
        assert!(head_str.ends_with("\r\n\r\n"));
        assert!(!head_str.contains("head-check"));
    }

    #[test]
    fn test_head_content_length_prefers_metadata() {
        let temp_dir = TempDir::new().expect("temp dir");
        let file_path = temp_dir.path().join("asset.bin");
        let body = vec![b'x'; 4096];
        std::fs::write(&file_path, &body).expect("write asset");

        let (length, source) =
            head_content_length_with_metadata_hint(&file_path, Ok(body.len() as u64))
                .expect("metadata length");

        assert_eq!(length, body.len());
        assert_eq!(source, HeadLengthSource::Metadata);
    }

    #[test]
    fn test_head_content_length_falls_back_when_metadata_missing() {
        let temp_dir = TempDir::new().expect("temp dir");
        let file_path = temp_dir.path().join("asset.bin");
        let body = vec![b'y'; 8192];
        std::fs::write(&file_path, &body).expect("write asset");

        let (length, source) = head_content_length_with_metadata_hint(
            &file_path,
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "metadata unavailable",
            )),
        )
        .expect("fallback length");

        assert_eq!(length, body.len());
        assert_eq!(source, HeadLengthSource::FallbackRead);
    }

    #[test]
    fn test_handle_request_head_large_file_content_length() {
        let temp_dir = TempDir::new().expect("temp dir");
        let site_dir = temp_dir.path();
        let body = vec![b'z'; 512 * 1024];
        std::fs::write(site_dir.join("index.html"), &body).expect("write index");

        let head_response = handle_request(site_dir, "HEAD / HTTP/1.1\r\n");
        let head_str = String::from_utf8_lossy(&head_response);

        assert_eq!(
            content_length(&head_str).expect("HEAD content-length"),
            body.len()
        );
        assert!(head_str.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_head_content_length_from_hint_or_fs_with_hint_skips_fs_lookup() {
        let missing_path = std::path::Path::new("/tmp/cass-preview-nonexistent-file-for-hint-test");
        let (length, source) = head_content_length_from_hint_or_fs(missing_path, Some(777))
            .expect("metadata hint should succeed without filesystem access");
        assert_eq!(length, 777);
        assert_eq!(source, HeadLengthSource::Metadata);
    }

    #[test]
    fn test_handle_request_with_site_root_precanonicalized() {
        let temp_dir = TempDir::new().expect("temp dir");
        let site_dir = temp_dir.path();
        std::fs::write(site_dir.join("index.html"), "<html>canonical</html>").expect("write index");
        let canonical_root = site_dir.canonicalize().expect("canonicalize root");

        let response = handle_request_with_site_root(&canonical_root, "GET / HTTP/1.1\r\n");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("HTTP/1.1 200 OK"));
        assert!(response_str.contains("canonical"));
    }

    #[test]
    fn test_handle_request_wrapper_accepts_uncanonicalized_site_dir() {
        let temp_dir = TempDir::new().expect("temp dir");
        let site_dir = temp_dir.path();
        std::fs::write(site_dir.join("index.html"), "<html>wrapper</html>").expect("write index");
        let dotted = site_dir.join(".");

        let response = handle_request(&dotted, "GET / HTTP/1.1\r\n");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("HTTP/1.1 200 OK"));
        assert!(response_str.contains("wrapper"));
    }
}
