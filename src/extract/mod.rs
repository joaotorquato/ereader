//! Extração de texto: PDF e EPUB viram a mesma estrutura `Vec<Section>`.

pub mod epub;
pub mod paragraphs;
pub mod pdf;

use anyhow::{bail, Result};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    /// 'h1'..'h6' | 'p' | 'li' | 'blockquote'
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub paragraphs: Vec<Paragraph>,
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: String,
    /// BCP-47 ('en', 'pt-BR'); vazio se desconhecido.
    pub lang: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Epub,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pdf => "pdf",
            Kind::Epub => "epub",
        }
    }
}

/// Detecta pelo conteúdo (magic bytes), com o nome só como desempate.
pub fn detect_kind(filename: &str, bytes: &[u8]) -> Option<Kind> {
    if bytes.starts_with(b"%PDF") {
        return Some(Kind::Pdf);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Some(Kind::Epub);
    }
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".pdf") {
        Some(Kind::Pdf)
    } else if lower.ends_with(".epub") {
        Some(Kind::Epub)
    } else {
        None
    }
}

pub struct ExtractOptions<'a> {
    /// Caminho do `pdftotext` (poppler) para o pipeline direto de fallback; `None` desliga.
    pub pdftotext: Option<&'a Path>,
    /// `ebook-convert` (Calibre): todo PDF passa primeiro por aqui.
    pub ebook_convert: &'a str,
    /// `epubcheck` pra validar o EPUB gerado; se reprovar, tenta outra versão de EPUB
    /// antes de desistir. `None` desliga a validação.
    pub epubcheck: Option<&'a Path>,
}

pub fn extract(
    kind: Kind,
    filename: &str,
    bytes: &[u8],
    opts: &ExtractOptions,
) -> Result<Extracted> {
    let fallback_title = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Sem título".into());
    let mut ex = match kind {
        Kind::Pdf => pdf::extract(bytes, opts)?,
        Kind::Epub => epub::extract(bytes)?,
    };
    if ex.title.trim().is_empty() {
        ex.title = fallback_title;
    }
    if ex.sections.is_empty() {
        bail!("nenhum texto legível encontrado");
    }
    Ok(ex)
}
