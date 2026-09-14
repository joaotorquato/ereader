//! Flags de linha de comando / ENV (clap). Cada flag aceita a ENV equivalente.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "reader",
    version,
    about = "Leitor de e-book com leitura em voz alta (Kokoro)"
)]
pub struct Config {
    /// Endereço de escuta
    #[arg(long, env = "READER_BIND", default_value = "0.0.0.0:8080")]
    pub bind: String,

    /// Diretório de dados (SQLite, livros originais, áudio gerado)
    #[arg(long, env = "READER_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Token de acesso (obrigatório). Aceito em `Authorization: Bearer`, cookie
    /// `reader_token` ou query `?token=`.
    #[arg(long, env = "READER_TOKEN")]
    pub token: String,

    /// Caminho do modelo ONNX do Kokoro
    #[arg(
        long,
        env = "READER_MODEL",
        default_value = "./models/kokoro-v1.0.onnx"
    )]
    pub model: PathBuf,

    /// Vozes: diretório com `<voz>.bin` (raw f32) ou arquivo NPZ `voices-v1.0.bin`
    #[arg(long, env = "READER_VOICES", default_value = "./models/voices")]
    pub voices: PathBuf,

    /// Threads intra-op do ONNX Runtime
    #[arg(long, env = "READER_THREADS", default_value_t = 2)]
    pub threads: usize,

    /// Voz padrão para livros em inglês
    #[arg(long, env = "READER_VOICE_EN", default_value = "af_heart")]
    pub voice_en: String,

    /// Voz padrão para livros em português
    #[arg(long, env = "READER_VOICE_PT", default_value = "pf_dora")]
    pub voice_pt: String,

    /// Executável do espeak-ng (ignorado com a feature `espeak-ffi`)
    #[arg(long, env = "READER_ESPEAK_BIN", default_value = "espeak-ng")]
    pub espeak_bin: String,

    /// Usar `pdftotext` (poppler) neste caminho em vez do pdf-extract
    #[arg(long, env = "READER_PDFTOTEXT")]
    pub pdftotext: Option<PathBuf>,

    /// Quantos chunks à frente pré-gerar
    #[arg(long, env = "READER_PREFETCH", default_value_t = 3)]
    pub prefetch: usize,

    /// Tamanho máximo de upload em MB
    #[arg(long, env = "READER_MAX_UPLOAD_MB", default_value_t = 200)]
    pub max_upload_mb: usize,
}
