use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try exact file match first
    if !path.is_empty() {
        if let Some(file) = StaticAssets::get(path) {
            let mime_type = file.metadata.mimetype();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime_type)],
                file.data,
            )
                .into_response();
        }
    }

    // SPA fallback: serve index.html for unmatched routes
    match StaticAssets::get("index.html") {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            file.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
