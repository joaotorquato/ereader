//! Token único. Rede privada (Tailscale), então isto é "não deixar o vizinho de
//! tailnet abrir por engano", não segurança de internet pública.
//!
//! Três lugares, porque cada um cobre um caso:
//!   * `Authorization: Bearer` — fetch() do app.js;
//!   * cookie `reader_token`   — `<audio src="/audio/...">` e o service worker,
//!                               que não conseguem mandar header;
//!   * `?token=`               — primeiro acesso: `/?token=xxx` no celular grava o
//!                               cookie (o app.js faz isso) e pronto.
//! Comparação em tempo constante para não ser bobo à toa.

use super::State;
use axum::extract::{Request, State as AxState};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const COOKIE: &str = "reader_token";

pub async fn require_token(AxState(state): AxState<State>, req: Request, next: Next) -> Response {
    if token_ok(&state.cfg.token, &req) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "token inválido",
        )
            .into_response()
    }
}

fn token_ok(expected: &str, req: &Request) -> bool {
    let headers = req.headers();
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v.strip_prefix("Bearer ") {
            if ct_eq(t.trim(), expected) {
                return true;
            }
        }
    }
    if let Some(cookies) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for c in cookies.split(';') {
            if let Some((k, v)) = c.trim().split_once('=') {
                if k == COOKIE && ct_eq(v.trim(), expected) {
                    return true;
                }
            }
        }
    }
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "token" && ct_eq(v, expected) {
                    return true;
                }
            }
        }
    }
    false
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aceita_header_cookie_e_query() {
        let mk = |h: Option<(&str, &str)>, uri: &str| {
            let mut b = Request::builder().uri(uri);
            if let Some((k, v)) = h {
                b = b.header(k, v);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        assert!(token_ok(
            "s3cret",
            &mk(Some(("authorization", "Bearer s3cret")), "/x")
        ));
        assert!(token_ok(
            "s3cret",
            &mk(Some(("cookie", "a=1; reader_token=s3cret")), "/x")
        ));
        assert!(token_ok("s3cret", &mk(None, "/x?y=1&token=s3cret")));
        assert!(!token_ok(
            "s3cret",
            &mk(Some(("authorization", "Bearer nope")), "/x")
        ));
        assert!(!token_ok("s3cret", &mk(None, "/x")));
    }
}
