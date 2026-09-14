#!/usr/bin/env bash
#
# serve.sh — sobe o reader local e publica em https://reader.passandplay.online
# via Cloudflare Tunnel (túnel nomeado, DNS fixo).
#
#   ./scripts/serve.sh            sobe servidor + túnel (Ctrl-C derruba os dois)
#   ./scripts/serve.sh --setup    primeira vez: login, cria túnel e rota DNS
#   HOST=outro.passandplay.online ./scripts/serve.sh   troca o hostname
#
# Token de acesso: lido de .env (READER_TOKEN=...); gerado na primeira execução.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
TUNNEL="${TUNNEL:-reader}"
HOST="${HOST:-reader.passandplay.online}"
BIND="${READER_BIND:-127.0.0.1:8080}"

if [ "${1:-}" = "--setup" ]; then
  [ -f ~/.cloudflared/cert.pem ] || cloudflared tunnel login   # abre o navegador; escolha a zona passandplay.online
  cloudflared tunnel list -o json | grep -q "\"name\":\"$TUNNEL\"" || cloudflared tunnel create "$TUNNEL"
  cloudflared tunnel route dns --overwrite-dns "$TUNNEL" "$HOST"
  echo "ok: $HOST -> túnel $TUNNEL"
  exit 0
fi

[ -f .env ] || { echo "READER_TOKEN=$(openssl rand -hex 24)" > .env; chmod 600 .env; echo "gerado .env com READER_TOKEN"; }
set -a; . ./.env; set +a
[ -x target/release/reader ] || cargo build --release

READER_BIND="$BIND" ./target/release/reader &
READER_PID=$!
trap 'kill "$READER_PID" 2>/dev/null' EXIT

echo "https://$HOST/?token=$READER_TOKEN"
exec cloudflared tunnel run --url "http://$BIND" "$TUNNEL"
