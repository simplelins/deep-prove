#!/bin/bash
# =============================================================================
# 优化后全量重测战役（复盘用）— 同一二进制顺序跑完全部测试点
#
# 用法:
#   cd /home/lww/project/06_zkp/deep-prove/zkml/model_cache
#   nohup ./retest_campaign.sh > campaign_$(date +%m%d_%H%M).log 2>&1 &
#   tail -f campaign_*.log        # 观察进度
#
# 设计:
#   - 顺序执行，单个测试失败/超时不中断战役（每个测试独立留痕）
#   - 每个测试产出: <name>.log（RUST_LOG=info 全日志）
#                   bench.<name>.csv（指标）
#                   summary.<name>.txt（自动提取的关键指标）
#   - 战役索引: $RUN_ROOT/INDEX.txt（每个测试的 exit code 与结果行）
#
# 时长预估（总计 ~14-18h，过夜）:
#   gpt2      128 冒烟 ~10min | 1024 最大可测 ~25min | 4096 架构拒绝 ~1min
#   gemma270m 512 ~5min | 1024 ~12min | 4096 ~1h | 8192 ~2h | 16384 ~3-4h
#   gemma-1b  512 → 预期 LFS 指针失败（权重未拉取，留痕记录）
#   llama-7b  512 ~1.9h | 1024 实际最大 ~4-7h
#   llama-7b  4096 不跑: T_pad≈1.9TB Fr、预估数天（依据记录在 INDEX）
# =============================================================================
set -u
cd "$(dirname "$0")"

BIN=/home/lww/project/06_zkp/deep-prove/target/release/bench-llm
RUN_ROOT="retest_$(date +%m%d_%H%M)"
mkdir -p "$RUN_ROOT"
BUDGET_MB=${BUDGET_MB:-512}
WINDOW=${WINDOW:-2}
MAX_SEG=${MAX_SEG:-48}
GIT_HEAD=$(git -C /home/lww/project/06_zkp/deep-prove rev-parse --short HEAD 2>/dev/null)
INDEX="$RUN_ROOT/INDEX.txt"

echo "campaign root: $RUN_ROOT  git: $GIT_HEAD  start: $(date)" | tee "$INDEX"

if [ ! -x "$BIN" ]; then
  echo "FATAL: $BIN missing" | tee -a "$INDEX"; exit 1
fi

# ---------------------------------------------------------------------------
# 单测试执行 + 留痕 + 汇总提取。参数: name model repo seq
# ---------------------------------------------------------------------------
run_one() {
  local name="$1" model="$2" repo="$3" seq="$4"
  echo "" | tee -a "$INDEX"
  echo ">>> [$name] start $(date) seq=$seq model=$model" | tee -a "$INDEX"
  local log="$RUN_ROOT/${name}.log"
  RUST_LOG=info stdbuf -oL -eL "$BIN" \
    --model "$model" --hf "$repo" --sequence "$seq" \
    --streaming --stream-budget-mb "$BUDGET_MB" --stream-window "$WINDOW" \
    --stream-max-segments "$MAX_SEG" \
    --prompt-file model_cache/long_prompt.txt \
    --bench "$RUN_ROOT/bench.${name}.csv" \
    > "$log" 2>&1
  local exit=$?
  echo "<<< [$name] exit=$exit end=$(date)" | tee -a "$INDEX"

  # 失败原因与 verify 逐段留痕（若有）
  grep -aE "panicked|streaming verify: chunk|Error:" "$log" | head -3 | sed 's/^/    /' >> "$INDEX"

  # 汇总提取
  local csv_file=$(ls -t "$RUN_ROOT"/bench.${name}*.csv 2>/dev/null | head -1)
  {
    echo "=== retest: $name seq=$seq model=$model exit=$exit git=$GIT_HEAD $(date) ==="
    echo "log: $log"
    echo "--- 分段 ---"
    grep -a "splitting the model in" "$log" | grep -oE "in [0-9]+ chunks"
    echo "--- run loop 打点（forward/dispatch/pin_release 分项）---"
    grep -aE "\[streaming\] run loop" "$log" | sed 's/.*zkml::model: //'
    echo "--- finish ---"
    grep -aE "\[streaming\] finish" "$log" | sed 's/.*zkml::model: //'
    echo "--- verify 逐段（首尾）与失败 ---"
    grep -aE "\[verify\] streaming chunk" "$log" | head -2 | sed 's/.*verifier: //'
    grep -aE "\[verify\] streaming chunk" "$log" | tail -2 | sed 's/.*verifier: //'
    grep -aE "streaming verify: chunk|panicked" "$log" | head -3
    echo "--- store 流量 ---"
    grep -aE "Store metrics after" "$log" | sed 's/.*bench_llm: //'
    if [ -n "$csv_file" ] && [ -f "$csv_file" ]; then
      echo "--- CSV 关键指标 ---"
      python3 - "$csv_file" << 'PYEOF'
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
  } > "$RUN_ROOT/summary.${name}.txt" 2>&1
  echo "    summary: $RUN_ROOT/summary.${name}.txt" | tee -a "$INDEX"
}

GPT2=openai-community/gpt2
G270=google/gemma-3-270m-it
G1B=google/gemma-3-1b-it
L7B=meta-llama/Llama-2-7b-hf

# 1) gpt2：冒烟 + 最大可测长度 1024 + 4096 架构拒绝留痕
run_one gpt2_s128   gpt2 "$GPT2" 128
run_one gpt2_s1024  gpt2 "$GPT2" 1024
run_one gpt2_s4096  gpt2 "$GPT2" 4096   # 预期架构拒绝（位置嵌入上限）

# 2) gemma-270m：全序列扫描（SWA=512，s>512 起 sliding window 生效）
for S in 512 1024 4096 8192 16384; do
  run_one "gemma270m_s${S}" gemma3 "$G270" "$S"
done

# 3) gemma-1b：预期 LFS 指针失败（环境限制留痕；权重拉取后可重测）
run_one g1b_s512 gemma3 "$G1B" 512

# 4) llama-7b：512（对照）+ 1024（实际最大可行长度）
#    注意 llama s=1024 内存峰值可能 ~500GB+，务必串行（本脚本即串行）
run_one llama7b_s512  llama2 "$L7B" 512
run_one llama7b_s1024 llama2 "$L7B" 1024

# llama-7b 4096：不执行。依据：T_pad≈486GB i64（F 文档 §7.4）→ ~1.9TB Fr，
# 即使 48 段每段 ~40GB Fr、预估证明数天——超出本战役范围，待 M2（表证明全局化）
# 与硬件加速后评估。

echo "" | tee -a "$INDEX"
echo "=== campaign complete: $(date) ===" | tee -a "$INDEX"
echo "结果目录: $RUN_ROOT/  （每个测试一个 summary.*.txt + log + csv）" | tee -a "$INDEX"
