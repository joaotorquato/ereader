#!/usr/bin/env bash
#
# disk-cleanup.sh — recupera espaço de artefatos de build regeneráveis.
#
#   barata  (sempre)    cache incremental, .DS_Store
#   fria    (14 dias)   target/ inteiro, se ninguém compilou há duas semanas
#
# Nunca toca em código, data/, models/ ou ~/.cargo/registry. Sai sempre com 0:
# é chamado do hook de SessionStart e não pode derrubar o início da sessão.
#
#   ./scripts/disk-cleanup.sh --dry-run     mostra o que sairia, não apaga
#   ./scripts/disk-cleanup.sh --force       ignora o throttle de 24h
#   ./scripts/disk-cleanup.sh --aggressive  apaga target/ agora, mais fontes
#                                           extraídas do registry e rust-docs.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$REPO_ROOT/target"
STAMP_FILE="$REPO_ROOT/.claude/.cleanup-stamp"
LOG_FILE="$REPO_ROOT/.claude/cleanup.log"

COLD_DAYS=14
THROTTLE_HOURS=24
LOG_MAX_LINES=300

DRY_RUN=0; FORCE=0; AGGRESSIVE=0; FREED_KB=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)    DRY_RUN=1 ;;
    --force)      FORCE=1 ;;
    --aggressive) AGGRESSIVE=1; FORCE=1 ;;
    -h|--help)    sed -n '2,14p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "flag desconhecida: $arg" >&2; exit 0 ;;
  esac
done

log() {
  printf '%s\n' "$1"
  printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$1" >>"$LOG_FILE" 2>/dev/null || true
}
kb_of() { [ -e "$1" ] && du -k -d 0 "$1" 2>/dev/null | awk 'NR==1{print $1+0}' || echo 0; }
human() {
  awk -v kb="$1" 'BEGIN {
    if (kb >= 1048576) printf "%.1f GiB", kb/1048576
    else if (kb >= 1024) printf "%.0f MiB", kb/1024
    else printf "%d KiB", kb }'
}
remove_path() {
  local path="$1" label="$2" kb
  [ -e "$path" ] || return 0
  kb=$(kb_of "$path"); [ "$kb" -eq 0 ] && return 0
  if [ "$DRY_RUN" -eq 1 ]; then log "  [dry-run] removeria $label ($(human "$kb"))"
  else rm -rf -- "$path" 2>/dev/null && log "  removido $label ($(human "$kb"))"; fi
  FREED_KB=$((FREED_KB + kb))
}

# Apagar incremental/ no meio de um build quebra o build.
build_running() { pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1 || pgrep -x rust-analyzer >/dev/null 2>&1; }

throttled() {
  [ "$FORCE" -eq 1 ] && return 1
  [ -f "$STAMP_FILE" ] || return 1
  [ $(( $(date +%s) - $(stat -f %m "$STAMP_FILE" 2>/dev/null || echo 0) )) -lt $((THROTTLE_HOURS * 3600)) ]
}

# deps/ e .fingerprint/ como sentinelas: 4 stats em vez de andar por target/
# inteiro, e a própria limpeza (que só remove incremental/) não reinicia o relógio.
target_age_days() {
  local newest=0 t
  for s in "$TARGET_DIR"/{debug,release}/{deps,.fingerprint}; do
    [ -e "$s" ] || continue
    t=$(stat -f %m "$s" 2>/dev/null || echo 0); [ "$t" -gt "$newest" ] && newest=$t
  done
  [ "$newest" -eq 0 ] && { echo -1; return; }
  echo $(( ($(date +%s) - newest) / 86400 ))
}

clean_cheap_layer() {
  remove_path "$TARGET_DIR/debug/incremental"   "cache incremental (debug)"
  remove_path "$TARGET_DIR/release/incremental" "cache incremental (release)"
  [ "$DRY_RUN" -eq 0 ] && find "$TARGET_DIR" -maxdepth 3 -name '.DS_Store' -delete 2>/dev/null
  return 0
}

# Extras que custam re-download, não rebuild.
clean_global_extras() {
  remove_path "$HOME/.cargo/registry/src" "fontes extraídas do cargo registry"
  for d in "$HOME"/.rustup/toolchains/*/share/doc; do remove_path "$d" "rust-docs offline"; done
}

mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null || true
throttled && exit 0
build_running && { log "limpeza adiada: build/rust-analyzer em execução"; exit 0; }

age=$(target_age_days)
log "limpeza iniciada (target com ${age}d desde o último build$([ "$DRY_RUN" -eq 1 ] && echo ', dry-run'))"

if [ "$AGGRESSIVE" -eq 1 ]; then
  remove_path "$TARGET_DIR" "target/ (agressivo)"
  clean_global_extras
elif [ "$age" -ge "$COLD_DAYS" ]; then
  remove_path "$TARGET_DIR" "target/ (frio >= ${COLD_DAYS}d)"
else
  clean_cheap_layer
fi

[ "$FREED_KB" -gt 0 ] && log "total: $(human "$FREED_KB")" || log "nada a limpar"
[ "$DRY_RUN" -eq 0 ] && touch "$STAMP_FILE" 2>/dev/null
[ -f "$LOG_FILE" ] && tail -n "$LOG_MAX_LINES" "$LOG_FILE" >"$LOG_FILE.tmp" 2>/dev/null && mv "$LOG_FILE.tmp" "$LOG_FILE"
exit 0
