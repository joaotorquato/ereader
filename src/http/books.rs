use super::{ApiError, ApiResult, State};
use crate::extract::{self, ExtractOptions};
use axum::extract::{Multipart, Path, State as AxState};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

pub async fn list(AxState(st): AxState<State>) -> ApiResult<Json<Vec<crate::db::Book>>> {
    Ok(Json(st.db.list_books()?))
}

#[derive(Serialize)]
pub struct BookDetail {
    #[serde(flatten)]
    book: crate::db::Book,
    sections: Vec<crate::db::Section>,
}

pub async fn show(AxState(st): AxState<State>, Path(id): Path<i64>) -> ApiResult<Json<BookDetail>> {
    let book = st
        .db
        .get_book(id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "livro não existe".into()))?;
    st.db.touch_book(id)?;
    let sections = st.db.list_sections(id)?;
    Ok(Json(BookDetail { book, sections }))
}

#[derive(Serialize)]
pub struct SectionDetail {
    id: i64,
    idx: i64,
    title: String,
    paragraphs: Vec<crate::db::Paragraph>,
}

pub async fn section(
    AxState(st): AxState<State>,
    Path((id, n)): Path<(i64, i64)>,
) -> ApiResult<Json<SectionDetail>> {
    let sec = st
        .db
        .get_section(id, n)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "seção não existe".into()))?;
    let paragraphs = st.db.list_paragraphs(sec.id)?;
    Ok(Json(SectionDetail {
        id: sec.id,
        idx: sec.idx,
        title: sec.title,
        paragraphs,
    }))
}

#[derive(Deserialize)]
pub struct Position {
    section: i64,
    paragraph: i64,
}

pub async fn set_position(
    AxState(st): AxState<State>,
    Path(id): Path<i64>,
    Json(p): Json<Position>,
) -> ApiResult<StatusCode> {
    st.db.set_position(id, p.section, p.paragraph)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete(AxState(st): AxState<State>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    match st.db.delete_book(id)? {
        Some(rel) => {
            let _ = std::fs::remove_file(st.cfg.data_dir.join(rel));
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(ApiError(StatusCode::NOT_FOUND, "livro não existe".into())),
    }
}

/// POST /books — multipart com campo `file`. A extração roda em `spawn_blocking`
/// porque um PDF de 500 páginas leva segundos e travaria o runtime.
pub async fn upload(
    AxState(st): AxState<State>,
    mut mp: Multipart,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let mut filename = String::from("livro");
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?
    {
        if field.name() == Some("file") {
            if let Some(n) = field.file_name() {
                filename = n.to_string();
            }
            let data = field
                .bytes()
                .await
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
            bytes = Some(data.to_vec());
        }
    }
    let bytes = bytes.ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "campo `file` ausente".into(),
    ))?;
    let kind = extract::detect_kind(&filename, &bytes).ok_or(ApiError(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "só PDF ou EPUB".into(),
    ))?;

    let cfg = st.cfg.clone();
    let fname = filename.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        let opts = ExtractOptions {
            pdftotext: cfg.pdftotext.as_deref(),
        };
        extract::extract(kind, &fname, &bytes, &opts).map(|ex| (ex, bytes))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let (ex, bytes) = extracted;

    // grava o original: books/<timestamp>-<nome-seguro>.<ext>
    let safe: String = filename
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(80)
        .collect();
    let rel = format!(
        "books/{}-{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        safe.trim_end_matches(".pdf").trim_end_matches(".epub"),
        kind.as_str()
    );
    let abs = st.cfg.data_dir.join(&rel);
    tokio::fs::create_dir_all(abs.parent().unwrap())
        .await
        .map_err(anyhow::Error::from)?;
    tokio::fs::write(&abs, &bytes)
        .await
        .map_err(anyhow::Error::from)?;

    let id = st
        .db
        .insert_book(&ex.title, &ex.lang, kind.as_str(), &rel, &ex.sections)?;
    tracing::info!(id, title = %ex.title, sections = ex.sections.len(), "livro importado");
    Ok((
        StatusCode::CREATED,
        Json(
            serde_json::json!({ "id": id, "title": ex.title, "lang": ex.lang, "sections": ex.sections.len() }),
        ),
    ))
}
