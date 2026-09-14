//! Style vectors das vozes do Kokoro.
//!
//! Cada voz é um tensor f32 [510, 1, 256]: para um texto de N tokens (sem pads)
//! o modelo recebe a linha N (`voice[N]`), 256 floats. Suportamos os dois formatos
//! em circulação:
//!
//!   * um arquivo raw por voz (`voices/af_heart.bin`, 522 240 bytes = 510×256×4,
//!     little-endian) — é o que o onnx-community publica. Carregamos sob demanda e
//!     só as vozes usadas ficam em memória (~520 KB cada).
//!   * `voices-v1.0.bin` do kokoro-onnx, que é um NPZ (zip de .npy, um por voz).
//!     Parsear .npy à mão são 30 linhas; não vale puxar ndarray + ndarray-npy
//!     (e o binário agradece).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const STYLE_DIM: usize = 256;
pub const STYLE_ROWS: usize = 510;

pub struct Voices {
    source: Source,
    cache: Mutex<HashMap<String, std::sync::Arc<Vec<f32>>>>,
}

enum Source {
    Dir(PathBuf),
    Npz(PathBuf),
}

impl Voices {
    /// `path` pode ser um diretório com `<voz>.bin` ou um arquivo NPZ.
    pub fn open(path: &Path) -> Result<Self> {
        let source = if path.is_dir() {
            Source::Dir(path.to_path_buf())
        } else if path.is_file() {
            Source::Npz(path.to_path_buf())
        } else {
            bail!("vozes não encontradas em {}", path.display());
        };
        Ok(Voices {
            source,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Nomes disponíveis (para o `GET /voices`).
    pub fn list(&self) -> Result<Vec<String>> {
        let mut names = match &self.source {
            Source::Dir(dir) => std::fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().map(|x| x == "bin").unwrap_or(false) {
                        p.file_stem().map(|s| s.to_string_lossy().to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            Source::Npz(file) => {
                let f = std::fs::File::open(file)?;
                let zip = zip::ZipArchive::new(f)?;
                zip.file_names()
                    .filter_map(|n| n.strip_suffix(".npy").map(|s| s.to_string()))
                    .collect()
            }
        };
        names.sort();
        Ok(names)
    }

    pub fn has(&self, name: &str) -> bool {
        self.list()
            .map(|l| l.iter().any(|n| n == name))
            .unwrap_or(false)
    }

    /// Vetor de estilo (256 floats) para `name` e `n_tokens` tokens (sem pads).
    pub fn style(&self, name: &str, n_tokens: usize) -> Result<Vec<f32>> {
        let all = self.load(name)?;
        let row = n_tokens.min(STYLE_ROWS - 1);
        let start = row * STYLE_DIM;
        Ok(all[start..start + STYLE_DIM].to_vec())
    }

    fn load(&self, name: &str) -> Result<std::sync::Arc<Vec<f32>>> {
        if let Some(v) = self.cache.lock().unwrap().get(name) {
            return Ok(v.clone());
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("nome de voz inválido: {name}");
        }
        let data = match &self.source {
            Source::Dir(dir) => {
                let p = dir.join(format!("{name}.bin"));
                let bytes =
                    std::fs::read(&p).with_context(|| format!("voz {name}: {}", p.display()))?;
                raw_f32_le(&bytes)?
            }
            Source::Npz(file) => {
                let f = std::fs::File::open(file)?;
                let mut zip = zip::ZipArchive::new(f)?;
                let mut entry = zip
                    .by_name(&format!("{name}.npy"))
                    .map_err(|_| anyhow!("voz {name} não existe em {}", file.display()))?;
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                parse_npy_f32(&bytes)?
            }
        };
        if data.len() != STYLE_ROWS * STYLE_DIM {
            bail!(
                "voz {name}: esperava {} floats, veio {}",
                STYLE_ROWS * STYLE_DIM,
                data.len()
            );
        }
        let arc = std::sync::Arc::new(data);
        self.cache
            .lock()
            .unwrap()
            .insert(name.to_string(), arc.clone());
        Ok(arc)
    }
}

fn raw_f32_le(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        bail!("tamanho não múltiplo de 4");
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Parser mínimo de .npy v1/v2: magic, versão, tamanho do header, header (dict
/// Python em texto), dados. Só aceitamos `<f4` C-order, que é o que o Kokoro usa.
fn parse_npy_f32(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() < 10 || &bytes[..6] != b"\x93NUMPY" {
        bail!("não é .npy");
    }
    let (major, _minor) = (bytes[6], bytes[7]);
    let (header_len, header_start) = match major {
        1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10),
        2 | 3 => (
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize,
            12,
        ),
        v => bail!("versão .npy {v} não suportada"),
    };
    let header = std::str::from_utf8(&bytes[header_start..header_start + header_len])?;
    if !header.contains("'<f4'") {
        bail!(".npy não é float32 little-endian: {header}");
    }
    if header.contains("'fortran_order': True") {
        bail!(".npy em ordem Fortran não suportado");
    }
    raw_f32_le(&bytes[header_start + header_len..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npy_minimo() {
        let mut header = String::from("{'descr': '<f4', 'fortran_order': False, 'shape': (2,), }");
        while (10 + header.len() + 1) % 64 != 0 {
            header.push(' ');
        }
        header.push('\n');
        let mut b = Vec::new();
        b.extend_from_slice(b"\x93NUMPY\x01\x00");
        b.extend_from_slice(&(header.len() as u16).to_le_bytes());
        b.extend_from_slice(header.as_bytes());
        b.extend_from_slice(&1.5f32.to_le_bytes());
        b.extend_from_slice(&(-2.0f32).to_le_bytes());
        assert_eq!(parse_npy_f32(&b).unwrap(), vec![1.5, -2.0]);
    }

    #[test]
    fn style_pega_a_linha_certa() {
        let dir = std::env::temp_dir().join(format!("reader-voices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut data = vec![0f32; STYLE_ROWS * STYLE_DIM];
        for (i, v) in data.iter_mut().enumerate() {
            *v = (i / STYLE_DIM) as f32; // cada linha vale seu índice
        }
        let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
        std::fs::write(dir.join("zz_test.bin"), bytes).unwrap();
        let voices = Voices::open(&dir).unwrap();
        assert!(voices.has("zz_test"));
        assert_eq!(voices.style("zz_test", 7).unwrap()[0], 7.0);
        assert_eq!(
            voices.style("zz_test", 9999).unwrap()[0],
            (STYLE_ROWS - 1) as f32
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
