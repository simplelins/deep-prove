#!/bin/bash
# 链式任务：等 gemma-1b 验证（512/1024）完成 → llama-2-13b s=128 → s=512
cd /home/lww/project/06_zkp/deep-prove/zkml/model_cache
echo "=== chain start: waiting for gemma-1b verification $(date) ==="
while pgrep -f "verify_g1b" > /dev/null || pgrep -f "bench-llm --model gemma3 --hf google/gemma-3-1b-it" > /dev/null; do sleep 120; done
echo "=== gemma-1b verification finished $(date) ==="

echo "=== llama-13b s=128 start $(date) ==="
./run_test.sh llama13b_s128
echo "=== llama-13b s=128 exit=$? $(date) ==="

echo "=== llama-13b s=512 start $(date) ==="
./run_test.sh llama13b_s512
echo "=== llama-13b s=512 exit=$? $(date) ==="
echo "=== chain complete $(date) ==="
