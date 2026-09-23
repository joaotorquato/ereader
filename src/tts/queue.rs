//! Fila de síntese com um único worker de inferência.
//!
//! Por que assim:
//!   * A CPU do Pi não ganha nada rodando duas inferências ao mesmo tempo — só
//!     perde cache e dobra a latência das duas. Um worker, ponto.
//!   * `spawn_blocking` em vez de `std::thread::spawn` porque queremos morrer junto
//!     com o runtime e porque `blocking_recv` já integra com o mpsc do tokio.
//!   * Dedup: se dois clientes (ou o prefetch e o clique do usuário) pedem o mesmo
//!     chunk, só uma inferência acontece; todos os `oneshot` esperando recebem o
//!     mesmo resultado. O mapa `waiting` é a única estrutura compartilhada, e vive
//!     atrás de um `std::sync::Mutex` (nunca seguramos o lock através de um await).
//!   * Prefetch tem prioridade menor de um jeito simples: cada job de prefetch
//!     carrega a `epoch` em que foi enfileirado; um pedido "de verdade" incrementa
//!     a epoch, e o worker descarta prefetches de epochs antigas (o cliente vai
//!     pedir de novo se precisar — e aí é um pedido real).

use super::kokoro::{self, Kokoro};
use super::phonemize::{Lang, Phonemizer};
use super::timing;
use super::voices::Voices;
use crate::db::{Clip, Db};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub text: String,
    pub voice: String,
    pub speed: f32,
    pub lang: Lang,
}

impl Request {
    /// sha256(texto ‖ voz ‖ velocidade). Velocidade com 2 casas para "1.0" e "1.00"
    /// caírem na mesma chave.
    pub fn key(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.text.as_bytes());
        h.update([0u8]);
        h.update(self.voice.as_bytes());
        h.update([0u8]);
        h.update(format!("{:.2}", self.speed).as_bytes());
        // Bump quando a síntese muda pra clips antigos não serem reaproveitados
        // (v2: pontuação passou a chegar no modelo — pausas/prosódia diferentes;
        //  v3: fonemização por palavra — timings e prosódia mudaram).
        h.update(b"\0v3");
        hex::encode(h.finalize())
    }
}

struct Job {
    req: Request,
    key: String,
}

/// Um pedido na fila: quem espera por ele e, se é prefetch, a epoch em que foi
/// pedido pela ÚLTIMA vez. Fica aqui (e não no `Job`) para um prefetch
/// re-pedido enquanto ainda está na fila continuar fresco — senão cada pedido
/// real invalidava os N-1 prefetches atrás dele e o pipeline caía pra 1.
struct Entry {
    waiters: Vec<oneshot::Sender<Result<Clip, String>>>,
    prefetch_epoch: Option<u64>,
}

type Waiters = HashMap<String, Entry>;

pub enum Pending {
    Ready(Clip),
    Waiting(oneshot::Receiver<Result<Clip, String>>),
}

impl Pending {
    pub async fn wait(self) -> Result<Clip, String> {
        match self {
            Pending::Ready(c) => Ok(c),
            Pending::Waiting(rx) => match rx.await {
                Ok(r) => r,
                Err(_) => Err("worker de TTS descartou o pedido".into()),
            },
        }
    }
}

pub struct TtsQueue {
    tx: mpsc::Sender<Job>,
    shared: Arc<Shared>,
    /// Estado que o worker expõe de volta (só leitura fora dele).
    pub info: Info,
}

/// Separado de `TtsQueue` para que o worker NUNCA segure o `tx`: se ele
/// guardasse um `Arc<TtsQueue>` inteiro, o canal nunca fecharia sozinho (o
/// próprio worker manteria seu remetente vivo) e `blocking_recv` travaria
/// para sempre no shutdown — só `kill -9` resolveria.
struct Shared {
    waiting: Mutex<Waiters>,
    epoch: AtomicU64,
    db: Arc<Db>,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub aligned: bool,
    pub voices: Vec<String>,
}

pub struct Deps {
    pub kokoro: Kokoro,
    pub voices: Voices,
    pub phonemizer: Box<dyn Phonemizer>,
    pub data_dir: PathBuf,
}

impl TtsQueue {
    pub fn start(deps: Deps, db: Arc<Db>, queue_len: usize) -> Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel::<Job>(queue_len.max(4));
        let info = Info {
            aligned: deps.kokoro.has_durations(),
            voices: deps.voices.list()?,
        };
        let shared = Arc::new(Shared {
            waiting: Mutex::new(HashMap::new()),
            epoch: AtomicU64::new(0),
            db,
        });
        let q = Arc::new(TtsQueue {
            tx,
            shared: shared.clone(),
            info,
        });
        tokio::task::spawn_blocking(move || worker(deps, shared, rx));
        Ok(q)
    }

    /// Pede um clip. Síncrono: enfileira (ou se pendura num pedido igual já na
    /// fila) e devolve um `Pending` para esperar. Separado do await para o chamador
    /// poder enfileirar prefetches DEPOIS deste job e ANTES de esperar por ele.
    pub fn request(&self, req: Request) -> Result<Pending, String> {
        let key = req.key();
        if let Ok(Some(c)) = self.shared.db.get_clip(&key) {
            return Ok(Pending::Ready(c));
        }
        self.shared.epoch.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let enqueue = {
            let mut w = self.shared.waiting.lock().unwrap();
            match w.get_mut(&key) {
                Some(e) => {
                    e.waiters.push(tx);
                    e.prefetch_epoch = None; // virou pedido real
                    false
                }
                None => {
                    w.insert(key.clone(), Entry { waiters: vec![tx], prefetch_epoch: None });
                    true
                }
            }
        };
        if enqueue {
            let job = Job { req, key: key.clone() };
            if let Err(e) = self.tx.try_send(job) {
                self.shared.waiting.lock().unwrap().remove(&key);
                return Err(match e {
                    mpsc::error::TrySendError::Full(_) => "fila de TTS cheia".into(),
                    mpsc::error::TrySendError::Closed(_) => "worker de TTS morreu".into(),
                });
            }
        }
        Ok(Pending::Waiting(rx))
    }

    /// Enfileira sem esperar. Não bloqueia: se a fila está cheia, ignora
    /// (prefetch é oportunista).
    pub fn prefetch(&self, req: Request) {
        let key = req.key();
        if let Ok(Some(_)) = self.shared.db.get_clip(&key) {
            return;
        }
        let epoch = self.shared.epoch.load(Ordering::Relaxed);
        {
            let mut w = self.shared.waiting.lock().unwrap();
            if let Some(e) = w.get_mut(&key) {
                if let Some(pe) = e.prefetch_epoch.as_mut() {
                    *pe = epoch; // já na fila: só renova
                }
                return;
            }
            w.insert(key.clone(), Entry { waiters: Vec::new(), prefetch_epoch: Some(epoch) });
        }
        let job = Job { req, key: key.clone() };
        if self.tx.try_send(job).is_err() {
            self.shared.waiting.lock().unwrap().remove(&key);
        }
    }

    pub fn is_cached(&self, key: &str) -> Option<Clip> {
        self.shared.db.get_clip(key).ok().flatten()
    }
}

fn worker(mut deps: Deps, shared: Arc<Shared>, mut rx: mpsc::Receiver<Job>) {
    tracing::info!(
        "worker de TTS pronto (alinhamento real: {})",
        deps.kokoro.has_durations()
    );
    while let Some(job) = rx.blocking_recv() {
        {
            // prefetch velho (ninguém re-pediu desde o último pedido real) → descarta
            let mut w = shared.waiting.lock().unwrap();
            let stale = w
                .get(&job.key)
                .and_then(|e| e.prefetch_epoch)
                .map(|e| e < shared.epoch.load(Ordering::Relaxed))
                .unwrap_or(false);
            if stale {
                w.remove(&job.key);
                continue;
            }
        }
        // alguém pode ter gerado enquanto estava na fila (não acontece com um worker,
        // mas custa uma query e protege contra restart com fila persistida no futuro)
        let result = match shared.db.get_clip(&job.key) {
            Ok(Some(c)) => Ok(c),
            _ => synthesize(&mut deps, &shared.db, &job).map_err(|e| {
                tracing::error!(key = %job.key, "síntese falhou: {e:#}");
                format!("{e:#}")
            }),
        };
        let waiters = shared
            .waiting
            .lock()
            .unwrap()
            .remove(&job.key)
            .map(|e| e.waiters)
            .unwrap_or_default();
        for w in waiters {
            let _ = w.send(result.clone());
        }
    }
    tracing::warn!("worker de TTS encerrando (canal fechado)");
}

fn synthesize(deps: &mut Deps, db: &Db, job: &Job) -> Result<Clip> {
    let started = std::time::Instant::now();
    let req = &job.req;

    let ph = deps.phonemizer.phonemize(&req.text, req.lang)?;
    if ph.phonemes.trim().is_empty() {
        anyhow::bail!("texto sem fonemas: {:?}", req.text);
    }

    let voices = &deps.voices;
    let voice = req.voice.clone();
    let out = deps
        .kokoro
        .synth(&ph.phonemes, |n| voices.style(&voice, n), req.speed)?;
    // speed do Kokoro acelera fala e pausas juntas; devolve só as pausas ao natural.
    let (samples, durations) = kokoro::restore_pause_speed(&out, req.speed);

    let rel = format!("audio/{}/{}.wav", &job.key[..2], job.key);
    let abs = deps.data_dir.join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap())?;
    let tmp = abs.with_extension("wav.tmp");
    let duration_ms = kokoro::write_wav(&tmp, &samples)?;
    std::fs::rename(&tmp, &abs).context("renomeando wav")?;

    let space_id = super::vocab::vocab()[&' '];
    let durations = durations.as_deref().map(|d| timing::Durations {
        per_token: d,
        token_ids: &out.token_ids,
        space_id,
    });
    let (timings, aligned) =
        timing::word_timings(&req.text, duration_ms, ph.per_word.as_deref(), durations);
    let timings_json = serde_json::to_string(&timings)?;

    db.insert_clip(
        &job.key,
        &req.text,
        &req.voice,
        req.speed,
        req.lang.code(),
        &rel,
        duration_ms as i64,
        &timings_json,
        aligned,
    )?;

    tracing::info!(
        key = &job.key[..8],
        chars = req.text.chars().count(),
        audio_ms = duration_ms,
        took_ms = started.elapsed().as_millis() as u64,
        "clip gerado"
    );
    Ok(Clip {
        key: job.key.clone(),
        path: rel,
        duration_ms: duration_ms as i64,
        word_timings: timings_json,
        aligned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chave_estavel_e_sensivel() {
        let a = Request {
            text: "Olá".into(),
            voice: "pf_dora".into(),
            speed: 1.0,
            lang: Lang::PtBr,
        };
        let b = Request {
            speed: 1.001,
            ..a.clone()
        };
        let c = Request {
            voice: "pm_alex".into(),
            ..a.clone()
        };
        assert_eq!(a.key(), b.key(), "1.0 e 1.001 arredondam igual");
        assert_ne!(a.key(), c.key());
        assert_eq!(a.key().len(), 64);
    }
}
