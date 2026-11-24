use axum::response::Html;
use std::path::Path;

const INDEX_HTML: &str = include_str!("static/index.html");

pub async fn index() -> Html<String> {
    // In local dev, read directly from disk if available for instant live-reload
    for path in ["rust/src/web/static/index.html", "src/web/static/index.html"] {
        if Path::new(path).exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                return Html(content);
            }
        }
    }
    Html(INDEX_HTML.to_string())
}
