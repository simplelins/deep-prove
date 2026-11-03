#!/bin/bash
# =============================================================================
# 全量重测战役：./run_all.sh [glob 模式]
#
# 用法:
#   nohup ./run_all.sh > campaign.log 2>&1 &     # 全量（~14-18h）
#   nohup ./run_all.sh 'gemma270m_*' > c.log 2>&1 &   # 只跑 gemma-270m 全序列
#   nohup ./run_all.sh 'llama*'      > c.log 2>&1 &   # 只跑 llama 系
#   tail -f campaign.log
#
# 顺序执行；单个测试的 PASS/FAIL 不中断战役，全部记入 INDEX.txt。
# 每个测试独立留痕（见 run_test.sh 头注释）。可随时 Ctrl-C 中断，
# 已完成的测试结果保留，续跑时换一个 RUN_ID 即可（结果目录不冲突）。
# =============================================================================
set -u
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PATTERN=${1:-*}
RUN_ID=$(date +%m%d_%H%M%S)
export RUN_ID
RESULTS_ROOT=${RESULTS_ROOT:-$(cd "$SCRIPT_DIR/../.." && pwd)/zkml/model_cache/results}
RUN_DIR="$RESULTS_ROOT/$RUN_ID"
mkdir -p "$RUN_DIR"
INDEX="$RUN_DIR/INDEX.txt"

GIT_HEAD=$(git -C "$SCRIPT_DIR/../.." rev-parse --short HEAD 2>/dev/null)
{
  echo "=== retest campaign: pattern='$PATTERN' git=$GIT_HEAD start=$(date) ==="
  echo "results: $RUN_DIR"
  echo ""
  echo "时间预算参考: gpt2 3项~35min | gemma270m 5项~6-8h | gemma1b 2项~3h"
  echo "              llama7b 2项~6-9h | 总计 ~14-18h（过夜）"
  echo ""
  echo "已知预期: gpt2_s4096 = 架构拒绝(PASS-expected-failure)"
  echo "          llama7b_s4096 = 不含在默认全量里（预估数天）"
} | tee "$INDEX"

PASS=0; FAIL=0; EXPECT=0; SKIP=0
for t in gpt2_s128 gpt2_s1024 gpt2_s4096 \
         gemma270m_s512 gemma270m_s1024 gemma270m_s4096 gemma270m_s8192 gemma270m_s16384 \
         gemma1b_s512 gemma1b_s4096 \
         llama7b_s512 llama7b_s1024; do
  [[ "$t" == $PATTERN ]] || continue
  echo "" | tee -a "$INDEX"
  bash "$SCRIPT_DIR/run_test.sh" "$t" 2>&1 | grep -aE "^=== \[|summary:" | tee -a "$INDEX"
  V=$(grep -aE "verdict=" "$INDEX" | grep "$t" | tail -1 | grep -oE "verdict=[A-Za-z-]+" | cut -d= -f2)
  case "$V" in
    PASS) PASS=$((PASS+1));;
    PASS-expected-failure) EXPECT=$((EXPECT+1));;
    UNEXPECTED-SUCCESS) FAIL=$((FAIL+1));;
    *) FAIL=$((FAIL+1));;
  esac
  echo "    [campaign 计分] PASS=$PASS FAIL=$FAIL EXPECTED=$EXPECT" | tee -a "$INDEX"
done

echo "" | tee -a "$INDEX"
echo "=== campaign complete: $(date)  PASS=$PASS FAIL=$FAIL EXPECTED-FAIL=$EXPECT ===" | tee -a "$INDEX"
echo "分析入口: $RUN_DIR/summary.*.txt（每个测试一份，含全部分项打点）" | tee -a "$INDEX"
