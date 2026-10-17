use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
struct Assets;

pub async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // The API router falls back here for unknown routes; an unknown API path
    // must be a 404, not a 200 with the SPA shell.
    if path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    match Assets::get(path) {
        // The MIME type is worked out at compile time by rust-embed's
        // `mime-guess` feature; looking it up at run time linked about 110 KB
        // of extension tables into the binary.
        Some(content) => {
            let mime = content.metadata.mimetype().to_owned();
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(content.data.into_owned()))
                .unwrap()
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for all unknown paths
            match Assets::get("index.html") {
                Some(content) => Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from(content.data.into_owned()))
                    .unwrap()
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}
