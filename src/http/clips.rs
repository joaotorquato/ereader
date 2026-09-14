use super::{ApiError, ApiResult, State};
use crate::db::Clip;
use crate::tts::chunk;
use crate::tts::queue::Request;
use crate::tts::resolve_voice;
use axum::extract::{Path, Query, State as AxState};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Deserialize)]
pub struct ClipsQuery {
    paragraph: i64,
    #[serde(default)]
    chunk: Option<usize>,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
}

#[derive(Serialize)]
pub struct ClipMeta {
    pub key: String,
    pub url: String,
    pub duration_ms: i64,
    pub word_timings: Box<RawValue>,
    pub aligned: bool,
}

#[derive(Serialize)]
pub struct ChunkInfo {
    idx: usize,
    offset: usize,
    text: String,
    key: String,
    /// preenchido só se já está em cache
    clip: Option<ClipMeta>,
}

#[derive(Serialize)]
pub struct ClipsList {
    paragraph: i64,
    voice: String,
    speed: f32,
    chunks: Vec<ChunkInfo>,
}

fn clamp_speed(s: Option<f32>) -> f32 {
    s.unwrap_or(1.0).clamp(0.5, 2.0)
}

fn meta(c: Clip) -> ClipMeta {
    let raw = RawValue::from_string(c.word_timings)
        .unwrap_or_else(|_| RawValue::from_string("[]".into()).unwrap());
    ClipMeta {
        url: format!("/{}", c.path),
        key: c.key,
        duration_ms: c.duration_ms,
        word_timings: raw,
        aligned: c.aligned,
    }
}

fn tts(st: &State) -> ApiResult<&std::sync::Arc<crate::tts::queue::TtsQueue>> {
    st.tts.as_ref().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "TTS indisponível neste servidor".into(),
    ))
}

/// Monta (voz, idioma, chunks) de um parágrafo.
fn plan(
    st: &State,
    paragraph_id: i64,
    voice: Option<&str>,
    speed: f32,
) -> ApiResult<(
    String,
    crate::tts::phonemize::Lang,
    Vec<chunk::Chunk>,
    Vec<Request>,
)> {
    let p = st.db.get_paragraph(paragraph_id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "parágrafo não existe".into(),
    ))?;
    let book_lang = st.db.paragraph_lang(paragraph_id)?.unwrap_or_default();
    let (voice, lang) = resolve_voice(&st.voice_defaults, &book_lang, voice);
    let chunks = chunk::split(&p.text);
    let reqs = chunks
        .iter()
        .map(|c| Request {
            text: c.text.clone(),
            voice: voice.clone(),
            speed,
            lang,
        })
        .collect();
    Ok((voice, lang, chunks, reqs))
}

pub async fn voices(AxState(st): AxState<State>) -> ApiResult<Json<serde_json::Value>> {
    let list = st
        .tts
        .as_ref()
        .map(|t| t.info.voices.clone())
        .unwrap_or_default();
    let aligned = st.tts.as_ref().map(|t| t.info.aligned).unwrap_or(false);
    Ok(Json(serde_json::json!({
        "available": st.tts.is_some(),
        "aligned": aligned,
        "voices": list,
        "default": { "en": st.voice_defaults.en, "pt": st.voice_defaults.pt },
    })))
}

/// GET /clips?paragraph=ID — lista de chunks com o que já está pronto, e dispara
/// prefetch dos primeiros N ainda não gerados.
pub async fn list(
    AxState(st): AxState<State>,
    Query(q): Query<ClipsQuery>,
) -> ApiResult<Json<ClipsList>> {
    let tts = tts(&st)?;
    let speed = clamp_speed(q.speed);
    let (voice, _lang, chunks, reqs) = plan(&st, q.paragraph, q.voice.as_deref(), speed)?;
    if let Some(v) = q.voice.as_deref().filter(|v| !v.is_empty()) {
        if !tts.info.voices.iter().any(|x| x == v) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("voz desconhecida: {v}"),
            ));
        }
    }
    let mut out = Vec::with_capacity(chunks.len());
    let mut queued = 0;
    for (c, r) in chunks.into_iter().zip(reqs) {
        let key = r.key();
        let cached = tts.is_cached(&key).map(meta);
        if cached.is_none() && queued < st.cfg.prefetch {
            tts.prefetch(r);
            queued += 1;
        }
        out.push(ChunkInfo {
            idx: c.idx,
            offset: c.offset,
            text: c.text,
            key,
            clip: cached,
        });
    }
    Ok(Json(ClipsList {
        paragraph: q.paragraph,
        voice,
        speed,
        chunks: out,
    }))
}

/// GET /clips/{key}?paragraph=ID&chunk=N — espera o clip ficar pronto e devolve
/// a meta. Também enfileira os próximos chunks (N+1..N+prefetch), atravessando
/// para os parágrafos seguintes da mesma seção.
pub async fn get(
    AxState(st): AxState<State>,
    Path(key): Path<String>,
    Query(q): Query<ClipsQuery>,
) -> ApiResult<Json<ClipMeta>> {
    let tts = tts(&st)?;
    let speed = clamp_speed(q.speed);
    let chunk_idx = q.chunk.ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "chunk obrigatório".into(),
    ))?;
    let (voice, lang, _chunks, reqs) = plan(&st, q.paragraph, q.voice.as_deref(), speed)?;
    let req = reqs.get(chunk_idx).cloned().ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "chunk fora do parágrafo".into(),
    ))?;
    if req.key() != key {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "key não corresponde a paragraph/chunk/voice/speed".into(),
        ));
    }

    // prefetch: resto deste parágrafo, depois parágrafos seguintes
    let mut ahead: Vec<Request> = reqs
        .into_iter()
        .skip(chunk_idx + 1)
        .take(st.cfg.prefetch)
        .collect();
    if ahead.len() < st.cfg.prefetch {
        if let Some(p) = st.db.get_paragraph(q.paragraph)? {
            for np in st.db.following_paragraphs(p.section_id, p.idx, 3)? {
                for c in chunk::split(&np.text) {
                    if ahead.len() >= st.cfg.prefetch {
                        break;
                    }
                    ahead.push(Request {
                        text: c.text,
                        voice: voice.clone(),
                        speed,
                        lang,
                    });
                }
                if ahead.len() >= st.cfg.prefetch {
                    break;
                }
            }
        }
    }

    // 1) o pedido real entra na fila primeiro; 2) os prefetches atrás dele;
    // 3) só então esperamos — assim o worker já emenda N+1 enquanto o cliente toca N.
    let pending = tts
        .request(req)
        .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, e))?;
    for r in ahead {
        tts.prefetch(r);
    }
    let clip = tokio::time::timeout(std::time::Duration::from_secs(90), pending.wait())
        .await
        .map_err(|_| ApiError(StatusCode::GATEWAY_TIMEOUT, "TTS demorou demais".into()))?
        .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(meta(clip)))
}
