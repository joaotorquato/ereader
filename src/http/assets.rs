//! Assets do front embutidos no binário via rust-embed (`assets/`).
//! Em debug ele lê do disco (edita e recarrega); em release vai dentro do ELF.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
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
            let mut body = f.data.into_owned();
            if path == "index.html" {
                // Cache-busting: o Cloudflare força max-age de 4h em .js/.css e o
                // celular ficava com versão velha após cada deploy.
                let mut html = String::from_utf8_lossy(&body).into_owned();
                for a in ["app.js", "style.css"] {
                    html = html.replace(&format!("/{a}\""), &format!("/{a}?v={}\"", version(a)));
                }
                body = html.into_bytes();
            }
            // insert (não AppendHeaders): o body Vec<u8> já põe application/octet-stream
            // e o append deixava dois content-type na resposta.
            let mut res = body.into_response();
            for (k, v) in headers {
                if let Ok(v) = header::HeaderValue::from_str(&v) {
                    res.headers_mut().insert(k, v);
                }
            }
            res
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 8 hex do sha256 do asset embutido (rust-embed já calcula).
fn version(path: &str) -> String {
    Assets::get(path)
        .map(|f| hex::encode(&f.metadata.sha256_hash()[..4]))
        .unwrap_or_default()
}
