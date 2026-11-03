#!/bin/bash
# =============================================================================
# 单测试运行器：./run_test.sh <test_id>
#
# 测试注册表见本文件下方 TESTS 定义（name|model|hf_repo|seq|expect）。
# 每次运行独立留痕到 results/ 目录：
#   results/<RUN_ID>/<test_id>.log       RUST_LOG=info 完整日志
#   results/<RUN_ID>/bench.<test_id>.*.csv  指标 CSV
#   results/<RUN_ID>/summary.<test_id>.txt  自动提取的分析汇总
#   results/<RUN_ID>/INDEX.txt           战役索引（run_all.sh 追加）
#
# expect 字段:
#   pass — 预期成功（exit=0 且 verify 通过）
#   fail — 预期失败（如 gpt2 s=4096 架构拒绝；失败才算符合预期）
# =============================================================================
set -u
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="$REPO_ROOT/target/release/bench-llm"
CACHE="$REPO_ROOT/zkml/model_cache"
RESULTS_ROOT=${RESULTS_ROOT:-$CACHE/results}
RUN_ID=${RUN_ID:-$(date +%m%d_%H%M%S)}
RUN_DIR="$RESULTS_ROOT/$RUN_ID"
mkdir -p "$RUN_DIR"

BUDGET_MB=${BUDGET_MB:-512}
WINDOW=${WINDOW:-2}
MAX_SEG=${MAX_SEG:-48}
GIT_HEAD=$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null)
PROMPT_FILE="$CACHE/long_prompt.txt"

TESTS=(
  "gpt2_s128|gpt2|openai-community/gpt2|128|pass|冒烟;内存~30GB;~10min"
  "gpt2_s1024|gpt2|openai-community/gpt2|1024|pass|gpt2 最大可测长度;~84GB;~25min"
  "gpt2_s4096|gpt2|openai-community/gpt2|4096|fail|预期架构拒绝(位置嵌入上限1024);~1min"
  "gemma270m_s512|gemma3|google/gemma-3-270m-it|512|pass|~56GB;~5min"
  "gemma270m_s1024|gemma3|google/gemma-3-270m-it|1024|pass|SWA 生效档;~8GB证;~12min"
  "gemma270m_s4096|gemma3|google/gemma-3-270m-it|4096|pass|rayon matmul 后可行;~1h"
  "gemma270m_s8192|gemma3|google/gemma-3-270m-it|8192|pass|~2h"
  "gemma270m_s16384|gemma3|google/gemma-3-270m-it|16384|pass|最长档;内存峰值或400GB;~3-4h"
  "gemma1b_s512|gemma3|google/gemma-3-1b-it|512|pass|权重已就位(sha256 3d4ef8d7);~40-70min"
  "gemma1b_s4096|gemma3|google/gemma-3-1b-it|4096|pass|4k 档;~2-3h"
  "llama7b_s512|llama2|meta-llama/Llama-2-7b-hf|512|pass|已测基线 1.88h(修复后)"
  "llama7b_s1024|llama2|meta-llama/Llama-2-7b-hf|1024|pass|llama 实际最大可行;内存或500GB+;~4-7h"
  "llama7b_s4096|llama2|meta-llama/Llama-2-7b-hf|4096|fail|预估数天(T_pad 1.9TB Fr);慎跑"
  "llama32_s512|llama2|LLM-Research/Llama-3.2-1B|512|pass|实验性:llama3 rope-scaling 支持性待验证"
)

lookup() {
  for t in "${TESTS[@]}"; do
    [[ "$t" == "$1|"* ]] && { echo "$t"; return 0; }
  done
  return 1
}

# ---------------------------------------------------------------------------
# 前置：二进制（缺失自动构建）、长 prompt 文件（缺失自动生成）
# ---------------------------------------------------------------------------
if [ ! -x "$BIN" ]; then
  echo "[setup] bench-llm 缺失，构建中..."
  (cd "$REPO_ROOT" && cargo build --release -p zkml --bin bench-llm) || exit 1
fi
if [ ! -s "$PROMPT_FILE" ]; then
  echo "[setup] 生成长 prompt 文本（供 --prompt-file 使用）"
  python3 -c "
para = ('The quick brown fox jumps over the lazy dog while the sun sets behind the hills. '
        'Zero knowledge proofs allow one party to convince another that a computation was '
        'performed correctly without revealing any secret information beyond the validity of '
        'the statement itself. Sumcheck protocols reduce polynomial evaluation claims to '
        'random point checks, and polynomial commitments bind the prover to fixed data. ')
open('$PROMPT_FILE', 'w').write(para * 400)
"
fi

TEST_ID=${1:-}
[ -z "$TEST_ID" ] && { echo "用法: $0 <test_id>"; echo "可用 test_id:"; for t in "${TESTS[@]}"; do echo "  ${t%%|*}"; done; exit 1; }

ROW=$(lookup "$TEST_ID") || { echo "未知 test_id: $TEST_ID"; exit 1; }
IFS='|' read -r NAME MODEL REPO SEQ EXPECT DESC <<< "$ROW"

# 权重与 tokenizer 预检（LFS 指针/缺失即 FATAL，并给出修复指引）
WEIGHT="$CACHE/$REPO/model.safetensors"
sz=$(stat -c%s "$WEIGHT" 2>/dev/null || echo 0)
if [ "$sz" -lt 1000000 ]; then
  echo "FATAL: $WEIGHT 缺失或为 LFS 指针（${sz}B）。"
  echo "修复: cd $CACHE && HF_ENDPOINT=https://hf-mirror.com ./download_missing_weights.sh"
  exit 1
fi
if [ ! -s "$CACHE/$REPO/tokenizer.json" ]; then
  echo "FATAL: $CACHE/$REPO/tokenizer.json 缺失（0 字节）"; exit 1
fi

# 内存预检（仅告警不阻断）
FREE_G=$(free -g | awk '/^Mem:/{print $7}')
NEED_G=350
[[ "$MODEL" == llama2 && "$SEQ" -ge 1024 ]] && NEED_G=550
[ "$FREE_G" -lt "$NEED_G" ] && echo "WARN: 可用内存 ${FREE_G}GB < ${NEED_G}GB，可能 OOM"

echo "=== [$TEST_ID] start $(date) seq=$SEQ model=$MODEL expect=$EXPECT ==="
echo "    $DESC"

LOG="$RUN_DIR/${TEST_ID}.log"
RUST_LOG=info stdbuf -oL -eL "$BIN" \
  --model "$MODEL" --hf "$REPO" --sequence "$SEQ" \
  --streaming --stream-budget-mb "$BUDGET_MB" --stream-window "$WINDOW" \
  --stream-max-segments "$MAX_SEG" \
  --prompt-file "$PROMPT_FILE" \
  --bench "$RUN_DIR/bench.${TEST_ID}.csv" \
  > "$LOG" 2>&1
EXIT=$?

VERIFY=$(grep -acE "\[verify\] streaming chunk" "$LOG" || true)
FAILMSG=$(grep -aE "streaming verify: chunk|panicked" "$LOG" | head -1 | cut -c1-180)

# expect 判定
VERDICT="PASS"
if [ $EXIT -ne 0 ]; then
  if [ "$EXPECT" == "fail" ]; then VERDICT="PASS-expected-failure"; else VERDICT="FAIL"; fi
else
  if [ "$EXPECT" == "fail" ]; then VERDICT="UNEXPECTED-SUCCESS"; fi
fi

# ---------------------------------------------------------------------------
# 汇总提取
# ---------------------------------------------------------------------------
CSV_FILE=$(ls -t "$RUN_DIR"/bench.${TEST_ID}*.csv 2>/dev/null | head -1)
{
  echo "=== retest: $TEST_ID seq=$SEQ model=$MODEL exit=$EXIT verdict=$VERDICT git=$GIT_HEAD $(date) ==="
  echo "desc: $DESC"
  echo "log: $LOG   csv: ${CSV_FILE:-none}"
  echo "--- 分段数 ---"
  grep -a "splitting the model in" "$LOG" | grep -oE "in [0-9]+ chunks"
  echo "--- run loop 打点（forward/dispatch/pin_release 分项）---"
  grep -aE "\[streaming\] run loop" "$LOG" | sed 's/.*zkml::model: //'
  echo "--- finish（排空+合并+IO）---"
  grep -aE "\[streaming\] finish" "$LOG" | sed 's/.*zkml::model: //'
  echo "--- verify 逐段（首尾各 2 行）与失败 ---"
  grep -aE "\[verify\] streaming chunk" "$LOG" | head -2 | sed 's/.*verifier: //'
  grep -aE "\[verify\] streaming chunk" "$LOG" | tail -2 | sed 's/.*verifier: //'
  [ -n "$FAILMSG" ] && echo "失败信息: $FAILMSG"
  echo "--- store 流量 ---"
  grep -aE "Store metrics after" "$LOG" | sed 's/.*bench_llm: //'
  if [ -n "$CSV_FILE" ] && [ -f "$CSV_FILE" ]; then
    echo "--- CSV 关键指标 ---"
    python3 - "$CSV_FILE" << 'PYEOF'
import csv, sys
with open(sys.argv[1]) as fh:
    rows = list(csv.reader(fh))
hdr = rows[0]
best = max(rows[1:], key=lambda r: sum(1 for c in r if c))
d = dict(zip(hdr, best))
for k in ['context_generation_time','streaming_prove_full','prove_full_memory_peak',
          'store_fetch_prove','store_disk_reads_prove','store_disk_read_mb_prove',
          'store_written_mb_tracegen','proof_size','verify_full_streaming']:
    if d.get(k):
        try:
            n = int(d[k])
            print(f'{k}: {n}  ({n/1000:.1f}s)' if ('time' in k or 'verify' in k or 'full' in k) else f'{k}: {n}')
        except ValueError:
            print(f'{k}: {d[k]}')
PYEOF
  fi
  echo "binary: $(ls -la $BIN | awk '{print $5, $6, $7, $8}')"
} > "$RUN_DIR/summary.${TEST_ID}.txt" 2>&1

echo "=== [$TEST_ID] exit=$EXIT verdict=$VERDICT ==="
echo "    summary: $RUN_DIR/summary.${TEST_ID}.txt"
[ "$VERDICT" == "FAIL" ] && grep -aE "Error:|panicked" "$LOG" | head -2
exit $([ "$VERDICT" == "PASS" ] || [ "$VERDICT" == "PASS-expected-failure" ] && echo 0 || echo 1)
