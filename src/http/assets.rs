//! Assets do front embutidos no binário via rust-embed (`assets/`).
//! Em debug ele lê do disco (edita e recarrega); em release vai dentro do ELF.

use axum::http::{header, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(|| async { serve("index.html") }))
        .route("/app.js", get(|| async { serve("app.js") }))
        .route("/style.css", get(|| async { serve("style.css") }))
        .route("/sw.js", get(|| async { serve("sw.js") }))
        .route(
            "/manifest.webmanifest",
            get(|| async { serve("manifest.webmanifest") }),
        )
        .route("/icon.svg", get(|| async { serve("icon.svg") }))
}

fn serve(path: &str) -> Response {
    match Assets::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut headers = vec![(header::CONTENT_TYPE, mime.as_ref().to_string())];
            // O SW precisa poder controlar "/" mesmo servido de /sw.js
            if path == "sw.js" {
                headers.push((
                    header::HeaderName::from_static("service-worker-allowed"),
                    "/".into(),
                ));
            }
            headers.push((header::CACHE_CONTROL, "no-cache".into()));
            (AppendHeaders(headers), f.data.into_owned()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
