#!/bin/bash
# =============================================================================
# 长序列流式证明测试脚本（4k/8k/16k）
#
# 用法:
#   ./run_4k_test.sh              # 默认: gemma-270m s=4096
#   ./run_4k_test.sh 8192         # 8k
#   ./run_4k_test.sh 16384 gemma3 google/gemma-3-270m-it
#
# 产出（全部落在 model_cache/ 下，文件名带时间戳，互不覆盖）:
#   bench_4k_test_s<SEQ>_<TS>.log      完整运行日志（RUST_LOG=info）
#   runlog/bench.4ktest.s<SEQ>_*..csv  指标 CSV（含 streaming_prove_full 等）
#   summary_4ktest_s<SEQ>_<TS>.txt     自动提取的关键指标（分析用汇总）
#
# 本脚本记录的关键分析数据:
#   - [streaming] run loop 打点: forward / complete_dispatch / pin_release 分项
#   - [streaming] finish 打点: 段证明排空+合并+IO 耗时
#   - [verify] streaming chunk 逐段验证日志（48 段全过 / 失败段号）
#   - store 流量计数: fetch / disk_reads（流式路径必须 disk_reads=0）
#   - prove_full_memory_peak 峰值内存
# =============================================================================
set -u
cd "$(dirname "$0")"

SEQ=${1:-4096}
MODEL=${MODEL:-gemma3}
HF_REPO=${HF_REPO:-google/gemma-3-270m-it}
BUDGET_MB=${BUDGET_MB:-512}
WINDOW=${WINDOW:-2}
MAX_SEG=${MAX_SEG:-48}
BIN=/home/lww/project/06_zkp/deep-prove/target/release/bench-llm
TS=$(date +%Y%m%d_%H%M%S)
LOG="bench_4ktest_s${SEQ}_${TS}.log"
CSV_BASE="bench.4ktest.s${SEQ}.${TS}"
SUMMARY="summary_4ktest_s${SEQ}_${TS}.txt"

echo "=== 4k test: model=$MODEL seq=$SEQ budget=${BUDGET_MB}MB window=$WINDOW max_seg=$MAX_SEG ==="
echo "=== start: $(date) ==="

if [ ! -x "$BIN" ]; then
  echo "ERROR: $BIN not found. Run: cargo build --release -p zkml --bin bench-llm"
  exit 1
fi

# 磁盘预检：context 生成期间内存峰值 ~300GB（gemma-270m 权重小，磁盘占用小，
# 但 llama 需 ~400GB RAM；磁盘主要被 /tmp 下的 store 页占用）
FREE_G=$(free -g | awk '/^Mem:/{print $7}')
if [ "$FREE_G" -lt 350 ]; then
  echo "WARN: available RAM ${FREE_G}GB < 350GB — context/prove may OOM"
fi

RUST_LOG=info stdbuf -oL -eL "$BIN" \
  --model "$MODEL" --hf "$HF_REPO" --sequence "$SEQ" \
  --streaming --stream-budget-mb "$BUDGET_MB" --stream-window "$WINDOW" \
  --stream-max-segments "$MAX_SEG" \
  --prompt-file model_cache/long_prompt.txt \
  --bench "runlog/${CSV_BASE}" \
  > "$LOG" 2>&1
EXIT=$?
echo "=== end: $(date) exit=$EXIT ==="

# ---------------------------------------------------------------------------
# 自动提取关键指标到 summary（后续分析入口）
# ---------------------------------------------------------------------------
CSV_FILE=$(ls -t runlog/${CSV_BASE}*.csv 2>/dev/null | head -1)
{
  echo "=== 4k test summary: seq=$SEQ model=$MODEL $(date) ==="
  echo "exit_code: $EXIT"
  echo "log: $LOG"
  echo "csv: ${CSV_FILE:-none}"
  echo ""
  echo "--- 分段与调度（run loop 打点）---"
  grep -aE "\[streaming\] run loop" "$LOG" | sed 's/.*zkml::model: //'
  echo ""
  echo "--- finish（排空+合并+IO）---"
  grep -aE "\[streaming\] finish" "$LOG" | sed 's/.*zkml::model: //'
  echo ""
  echo "--- 段数 ---"
  grep -a "splitting the model in" "$LOG" | grep -oE "in [0-9]+ chunks"
  echo ""
  echo "--- verify 逐段（首尾各 2 行 + 失败行）---"
  grep -aE "\[verify\] streaming chunk" "$LOG" | head -2 | sed 's/.*verifier: //'
  grep -aE "\[verify\] streaming chunk" "$LOG" | tail -2 | sed 's/.*verifier: //'
  grep -aE "streaming verify: chunk|panicked" "$LOG" | head -3
  echo ""
  echo "--- store 流量（必须 disk_reads=0）---"
  grep -aE "Store metrics after" "$LOG" | sed 's/.*bench_llm: //'
  echo ""
  if [ -n "$CSV_FILE" ] && [ -f "$CSV_FILE" ]; then
    echo "--- CSV 关键指标（毫秒/字节）---"
    python3 - "$CSV_FILE" << 'PYEOF'
import csv, sys
with open(sys.argv[1]) as fh:
    rows = list(csv.reader(fh))
hdr = rows[0]
best = max(rows[1:], key=lambda r: sum(1 for c in r if c))
d = dict(zip(hdr, best))
keys = ['context_generation_time','streaming_prove_full','prove_full_memory_peak',
        'store_fetch_prove','store_disk_reads_prove','store_disk_read_mb_prove',
        'store_written_mb_tracegen','proof_size','verify_full_streaming']
for k in keys:
    if d.get(k):
        v = d[k]
        try:
            n = int(v)
            print(f'{k}: {n}  ({n/1000:.1f}s)' if ('time' in k or 'verify' in k or 'full' in k) else f'{k}: {n}')
        except ValueError:
            print(f'{k}: {v}')
PYEOF
  fi
  echo ""
  echo "--- 环境 ---"
  echo "git: $(git -C /home/lww/project/06_zkp/deep-prove rev-parse --short HEAD 2>/dev/null)"
  echo "binary: $(ls -la $BIN | awk '{print $5, $6, $7, $8}')"
  echo "mem_total: $(free -g | awk '/^Mem:/{print $2}')GB"
} > "$SUMMARY" 2>&1

echo "=== summary written to $SUMMARY ==="
cat "$SUMMARY"
