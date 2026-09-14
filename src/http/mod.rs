//! Camada HTTP (axum 0.8). Rotas:
//!
//!   GET  /                           shell da PWA (embutido)
//!   GET  /app.js /style.css /sw.js /manifest.webmanifest /icon.svg
//!   GET  /health
//!   GET  /voices                     vozes disponíveis + padrão por idioma
//!   GET  /books                      lista
//!   POST /books                      multipart `file` (pdf/epub)
//!   GET  /books/{id}                 meta + lista de seções
//!   DELETE /books/{id}
//!   GET  /books/{id}/sections/{n}    {title, paragraphs:[{id, idx, kind, text}]}
//!   PUT  /books/{id}/position        {section, paragraph}
//!   GET  /clips?paragraph=ID[&voice=][&speed=]   lista de chunks (sem gerar)
//!   GET  /clips/{key}?paragraph=&chunk=[&voice=][&speed=]  espera gerar → meta
//!   GET  /audio/{key}.wav            arquivo (com Range — Safari exige)
//!
//! Tudo menos `/health` e o shell estático passa pelo middleware de token.

pub mod assets;
pub mod auth;
pub mod books;
pub mod clips;

use crate::config::Config;
use crate::db::Db;
use crate::tts::queue::TtsQueue;
use crate::tts::VoiceDefaults;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{middleware, Json, Router};
use std::sync::Arc;
use tower_http::services::ServeDir;

pub struct AppState {
    pub db: Arc<Db>,
    pub tts: Option<Arc<TtsQueue>>,
    pub cfg: Config,
    pub voice_defaults: VoiceDefaults,
}

pub type State = Arc<AppState>;

/// Erro de handler → resposta JSON `{error}`. `anyhow::Error` vira 500;
/// os handlers constroem `ApiError` explícito para 4xx.
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.1 }));
        (self.0, body).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: State) -> Router {
    let audio_dir = state.cfg.data_dir.join("audio");
    let max_body = state.cfg.max_upload_mb * 1024 * 1024;

    let api = Router::new()
        .route("/voices", get(clips::voices))
        .route("/books", get(books::list).post(books::upload))
        .route("/books/{id}", get(books::show).delete(books::delete))
        .route("/books/{id}/sections/{n}", get(books::section))
        .route("/books/{id}/position", put(books::set_position))
        .route("/clips", get(clips::list))
        .route("/clips/{key}", get(clips::get))
        // ServeDir cuida de Range/206, ETag e Content-Type do .wav
        .nest_service("/audio", ServeDir::new(audio_dir))
        .layer(DefaultBodyLimit::max(max_body))
        // `.layer` (não `.route_layer`) para cobrir também o nest_service de /audio
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ));

    Router::new()
        .route("/health", get(health))
        .merge(assets::router())
        .merge(api)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}
