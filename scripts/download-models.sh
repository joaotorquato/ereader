#!/usr/bin/env bash
# Baixa o Kokoro-82M v1.0 em ONNX e as vozes para ./models (ou $1).
#
#   ./scripts/download-models.sh            # int8 + timestamps (recomendado)
#   VARIANT=fp32 ./scripts/download-models.sh
#   VOICES="af_heart pf_dora pm_alex" ./scripts/download-models.sh
#
# Modelo: onnx-community/Kokoro-82M-v1.0-ONNX-timestamped — mesma rede, com a
# saída extra `durations` (frames por token) que o reader usa para alinhar as
# palavras de verdade. Variantes (tamanho aproximado):
#   quantized (int8 dinâmico)  ~92 MB   ← default; melhor custo/benefício em CPU
#   q8f16                      ~86 MB   (pesos f16; no Pi costuma ser mais lento)
#   fp16                      ~163 MB
#   fp32                      ~326 MB   (referência de qualidade)
# Vozes: um .bin raw f32 [510,1,256] por voz, do repo onnx-community/Kokoro-82M-v1.0-ONNX.
set -euo pipefail

DEST="${1:-./models}"
VARIANT="${VARIANT:-quantized}"
VOICES="${VOICES:-af_heart af_bella am_michael bf_emma pf_dora pm_alex pm_santa}"
HF="https://huggingface.co"

case "$VARIANT" in
  quantized) FILE="model_quantized.onnx" ;;
  q8f16)     FILE="model_q8f16.onnx" ;;
  fp16)      FILE="model_fp16.onnx" ;;
  fp32)      FILE="model.onnx" ;;
  *) echo "VARIANT inválido: $VARIANT" >&2; exit 1 ;;
esac

mkdir -p "$DEST/voices"

fetch() { # url dest
  if [ -s "$2" ]; then echo "já existe: $2"; return; fi
  echo "baixando $1"
  curl -fL --progress-bar -o "$2.part" "$1" && mv "$2.part" "$2"
}

fetch "$HF/onnx-community/Kokoro-82M-v1.0-ONNX-timestamped/resolve/main/onnx/$FILE" \
      "$DEST/kokoro-v1.0.onnx"

for v in $VOICES; do
  fetch "$HF/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/$v.bin" "$DEST/voices/$v.bin"
done

echo
echo "pronto:"
ls -la "$DEST" "$DEST/voices"
echo
echo "rode com: READER_MODEL=$DEST/kokoro-v1.0.onnx READER_VOICES=$DEST/voices"
