#!/bin/bash
# =============================================================================
# 一键下载 model_cache 中缺失的权重（LFS 指针文件的真实内容）
#
# 用法:
#   ./download_missing_weights.sh            # 下载全部
#   ./download_missing_weights.sh g1b tiny   # 只下载指定项
#
# 环境变量:
#   HF_TOKEN   gemma-3-1b-it 是 gated 模型，需要有效 token（账号须已接受许可）
#   HF_ENDPOINT 默认 https://huggingface.co ；国内可设 https://hf-mirror.com
#
# 每个文件下载后自动 sha256 校验（校验和取自本目录 LFS 指针文件的 oid），
# 校验失败自动删除坏文件并报错——绝不留下静默损坏的权重。
# =============================================================================
set -u
cd "$(dirname "$0")"
: "${HF_ENDPOINT:=https://huggingface.co}"
AUTH=()
[ -n "${HF_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $HF_TOKEN")

fetch() {  # fetch <out_path> <url> <expect_sha256>
  local out="$1" url="$2" want="$3"
  if [ -f "$out" ] && [ "$(stat -c%s "$out")" -gt 1000000 ]; then
    echo "[SKIP] $out 已存在"
    return 0
  fi
  echo "[DL] $url"
  curl -L ${AUTH[@]+"${AUTH[@]}"} --fail --retry 3 -o "$out.part" "$url" || {
    echo "[FAIL] 下载失败: $out"; rm -f "$out.part"; return 1; }
  local got
  got=$(sha256sum "$out.part" | awk '{print $1}')
  if [ "$got" != "$want" ]; then
    echo "[FAIL] sha256 不匹配: $out (got $got want $want)"; rm -f "$out.part"; return 1
  fi
  mv "$out.part" "$out"
  echo "[OK] $out ($(stat -c%s "$out") bytes, sha256 verified)"
}

WANT=${*:-g1b tiny gpt2q8 g270q8 gpt2q2}

[[ " $WANT " == *" g1b "* ]] && {
  echo "=== gemma-3-1b-it (1.86GB, gated: 需 HF_TOKEN 且账号已接受 google 许可) ==="
  fetch "google/gemma-3-1b-it/model.safetensors" \
    "$HF_ENDPOINT/google/gemma-3-1b-it/resolve/main/model.safetensors" \
    "3d4ef8d71c14db7e448a09ebe891cfb6bf32c57a9b44499ae0d1c098e48516b6"
}

[[ " $WANT " == *" tiny "* ]] && {
  echo "=== TinyLlama-1.1B-Chat-v1.0 (2.2GB, ungated) ==="
  fetch "TinyLlama/TinyLlama-1.1B-Chat-v1.0/model.safetensors" \
    "$HF_ENDPOINT/TinyLlama/TinyLlama-1.1B-Chat-v1.0/resolve/main/model.safetensors" \
    "6e6001da2106d4757498752a021df6c2bdc332c650aae4bae6b0c004dcf14933"
}

[[ " $WANT " == *" gpt2q8 "* ]] && {
  echo "=== gpt2.Q8_0.gguf (176MB, ungated; 源见 zkml/src/parser/gguf.rs:399 注释) ==="
  fetch "gpt2.Q8_0.gguf" \
    "$HF_ENDPOINT/igorbkz/gpt2-Q8_0-GGUF/resolve/main/gpt2.q8_0.gguf" \
    "4d1df054e273ac10b3a31a2f90660426212d033156bb0b530e7517662fbe0c32"
  # 5a03305c....gguf 与 gpt2.Q8_0 是同一文件（oid 相同）的哈希命名副本
  cp -n gpt2.Q8_0.gguf 5a03305c6e23aab18e7052a9aa806375f8800ed86dbd8ac36e27cd68d7e8face.gguf 2>/dev/null
}

[[ " $WANT " == *" g270q8 "* ]] && {
  echo "=== gemma-3-270m-it-Q8_0.gguf (291MB; 候选源 unsloth，若 404 请核对本机 .hfd 记录) ==="
  fetch "gemma-3-270m-it-Q8_0.gguf" \
    "$HF_ENDPOINT/unsloth/gemma-3-270m-it-GGUF/resolve/main/gemma-3-270m-it-Q8_0.gguf" \
    "d156a5159f2f79c1b1d53c7c1cc20f1ff28ab8d00f17a292620aad13399b9698"
}

[[ " $WANT " == *" gpt2q2 "* ]] && {
  echo "=== gpt2.Q2_K.gguf (81MB; 候选源，若 404 请核对) ==="
  fetch "gpt2.Q2_K.gguf" \
    "$HF_ENDPOINT/igorbkz/gpt2-Q8_0-GGUF/resolve/main/gpt2.Q2_K.gguf" \
    "361dc033763f6698a5b21204692f5aec0622102796115c0392f1acb167ae1de4"
}

echo "=== done $(date)。校验失败的文件已删除，可重跑本脚本重试 ==="
