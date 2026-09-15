mod config;
mod db;
mod extract;
mod http;
mod tts;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,ort=warn")),
        )
        .compact()
        .init();

    let mut cfg = config::Config::parse();
    if cfg.token.len() < 8 {
        anyhow::bail!("READER_TOKEN precisa ter pelo menos 8 caracteres");
    }
    if cfg.pdftotext.is_none() {
        if let Some(p) = config::detect_pdftotext_on_path() {
            tracing::info!(path = %p.display(), "pdftotext (poppler) achado no PATH; preferindo em vez do pdf-extract puro-Rust");
            cfg.pdftotext = Some(p);
        }
    }

    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("criando {}", cfg.data_dir.display()))?;
    for sub in ["books", "audio"] {
        std::fs::create_dir_all(cfg.data_dir.join(sub))?;
    }
    let db = Arc::new(db::Db::open(&cfg.data_dir.join("reader.sqlite"))?);

    // TTS é opcional de propósito: sem modelo o app ainda abre livros e o front
    // cai para Web Speech (recebe 503 em /clips).
    let tts = match start_tts(&cfg, db.clone()) {
        Ok(q) => Some(q),
        Err(e) => {
            tracing::warn!("TTS desligado: {e:#}");
            None
        }
    };

    let state: http::State = Arc::new(http::AppState {
        db,
        tts,
        voice_defaults: tts::VoiceDefaults {
            en: cfg.voice_en.clone(),
            pt: cfg.voice_pt.clone(),
        },
        cfg: cfg.clone(),
    });

    let app = http::router(state).layer(tower_http::trace::TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("bind {}", cfg.bind))?;
    tracing::info!("escutando em http://{}", cfg.bind);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn start_tts(cfg: &config::Config, db: Arc<db::Db>) -> Result<Arc<tts::queue::TtsQueue>> {
    let kokoro = tts::kokoro::Kokoro::load(&cfg.model, cfg.threads)?;
    let voices = tts::voices::Voices::open(&cfg.voices)?;
    for v in [&cfg.voice_en, &cfg.voice_pt] {
        if !voices.has(v) {
            anyhow::bail!("voz padrão `{v}` não existe em {}", cfg.voices.display());
        }
    }
    let phonemizer = make_phonemizer(cfg)?;
    let deps = tts::queue::Deps {
        kokoro,
        voices,
        phonemizer,
        data_dir: cfg.data_dir.clone(),
    };
    tts::queue::TtsQueue::start(deps, db, 64)
}

#[cfg(feature = "espeak-ffi")]
fn make_phonemizer(_cfg: &config::Config) -> Result<Box<dyn tts::phonemize::Phonemizer>> {
    Ok(Box::new(tts::phonemize::ffi::Ffi::new()?))
}

#[cfg(not(feature = "espeak-ffi"))]
fn make_phonemizer(cfg: &config::Config) -> Result<Box<dyn tts::phonemize::Phonemizer>> {
    let cli = tts::phonemize::Cli::new(cfg.espeak_bin.clone());
    cli.check()?;
    Ok(Box::new(cli))
}

async fn shutdown() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("encerrando");
}
