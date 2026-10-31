# 多模型多序列流式证明测试报告

## 文档控制

| 项目 | 内容 |
| --- | --- |
| 文档编号 | M |
| 所属系列 | zkml_bootcamp |
| 版本 | 1.0 |
| 发布日期 | 2026-10-31 |
| 文档状态 | 已发布 |

---

## 1 测试矩阵与结果

模型可用性：`zkml/model_cache` 中真实权重仅 **gpt2**、**gemma-3-270m-it**、**Llama-2-7b**（gemma-3-1b / Llama-3.2-1B / TinyLlama / 两个 gguf 均为未拉取的 git-lfs 指针——已加检测，报错可操作）。

| # | 模型 | 序列 | 模式 | 结果 | 备注 |
| --- | --- | --- | --- | --- | --- |
| 1 | gpt2 124M | 128 | 完整生成+移位校验 | ✅ | 融合证明 197s，内存 28GB，disk_reads=0 |
| 2 | gpt2 | 512 | 完整生成+移位校验 | ✅ | 融合 650s，内存 58.6GB |
| 3 | gpt2 | 1024 | 完整生成+移位校验 | ✅ | 融合 1343s，内存 83.9GB |
| 4 | gpt2 | 4096 | — | ✅ 干净拒绝 | 架构上限 1024（位置嵌入），报错清晰 |
| 5 | gemma-270m (SWA512) | 512 | 完整生成+移位校验 | ✅ | 融合 1933s（与 gpt2 测试并行争核） |
| 6 | gemma-270m | 512 | prompt 模式 | ✅ | 融合 276s |
| 7 | gemma-270m | 768 | prompt 模式 | ✅ | SWA 生效（s>512） |
| 8 | gemma-270m | 1024 | prompt 模式 | ⚠️ 见 §3 | 16 段；偶发 verify 失败（r2），r1/r3 通过 |
| 9 | gemma-3-1b | 512 | — | ❌ 已修 | 权重是 LFS 指针（不可测）；config 兼容性 bug 已修 |
| 10 | llama-2-7b | 512 | prompt 模式 | ✅ | **125 段**流水线 14.15h 稳定运行，verify 通过；段固定开销主导（见 §6） |

## 2 发现并已修复的问题

| # | 问题 | 修复 | 提交 |
| --- | --- | --- | --- |
| F1 | **单大多项式 MSM 单线程**：大词表模型的 logits 段（gemma [512, 262144] = 5.4GB Fr）在 `batch_poly_msm_cpu` 中只有多项式间并行，单个大多项式的 MSM 单线程跑 ~50 分钟 | vendor 补丁：`batch_poly_msm_cpu` 对 >2²³ 项的多项式按标量索引分块、rayon 并行、部分和相加（MSM 线性，数学等价）；`[patch]` 接线，回归测试通过 | 99c3d062 |
| F2 | **gemma-3 老版 config 兼容**：gemma-3-1b-it 的 config.json 缺 `layer_types`，parser 硬性要求 → 加载失败 | 回退到架构默认模式（每 6 层第 6 层 full attention，与新版 config 的实际值一致） | d21e3a2f |
| F3 | **LFS 指针文件混淆报错**：未拉取的权重（135 字节文本）导致 `header too large` 等费解错误 | `from_hugging_face_cached` 检测 LFS 指针 magic，报可操作错误 | d21e3a2f |
| F4 | 观测缺失：bench 日志被 EnvFilter 过滤（无 RUST_LOG 时 INFO 全丢）、verify 失败只有裸 panic 无段号 | 压测脚本加 RUST_LOG；`verify_streaming` 逐段 catch_unwind，失败报告 chunk idx/id/节点数；run 循环加 forward/dispatch/pin 释放分项计时 | d21e3a2f |

## 3 未决问题：偶发 verify 失败（P1）

**现象**：gemma-270m s=1024（16 段）的 `verify_streaming` **偶发**失败——压测 3 轮中 r2 失败、r1/r3 通过；MSM 补丁前后各出现过。失败点**固定模式**：sumcheck verifier 第 0 轮"prover message not consistent with the claim"（进入 sumcheck 前 transcript 已分叉）。压测捕获的现场：`chunk idx 9 (Chunk(6), 44 节点)`；r3 同段通过。

**已排除**：SWA 生效（s=768 通过）、边界承诺顺序（BTreeMap）、表 multiplicity 顺序（BTreeMap table_column 序）、opening 顺序（两侧均 BTreeMap）、模型 IO 锚定、pipeline 投递顺序（FIFO=吸收序=Vec 序）。

**当前怀疑**：Chunk(6) 段内某个**吸收进 transcript 的元素存在进程级随机顺序**（RandomState），或 dp-crypto sumcheck prover 多 worker 与 transcript 交互的顺序非确定。诊断工具已就位（逐段 panic 定位 + INFO 日志 + 压测脚本 `model_cache/stress_s1024.sh`，失败率约 1/3–2/3，单轮 ~50 分钟）。

**影响评估**：**可靠性缺陷**（非安全缺陷——失败是"诚实的证明被拒绝"，不是"伪造证明被接受"）。经典单段路径（`Driver::prove`）未观察到失败。

**下一步**：① 以 Chunk(6) 为最小复现做段级 diff（dump prover/verifier 两侧 transcript 的吸收序列）；② 审计 `prove_chunk` 内所有进 transcript 的迭代容器；③ 检查 dp-crypto sumcheck prover 多 worker 路径。

## 4 长序列的硬约束（CPU 后端）

**burn-ndarray 的 i64 matmul 是朴素单线程实现**（multi-threads feature 仅对浮点 gemm 生效）：gemma-270m s=1024 的**单次前向 57 分钟**（run 循环打点 forward=3421s，流式证明只增加 ~5 分钟）。外推：s=4096 前向 ~15h、s=8192 ~60h、s=16384 ~240h——**4k+ 的 CPU 端到端测试在当前实现下不可行**，与流式证明无关。可行路径：① CUDA 后端（`--features cuda`，前向 GPU 化）；② burn/candle 的多线程 i64 gemm；③ G 文档的 GPU 实测路线。

## 5 性能数据汇总（流式 pipeline, window=2, budget=512MB）

### 5.1 llama-7b 证明性能异常（P2，待专项 profiling）

后续归因运行（llama-7b s=128，13 段）揭示了**独立于流式的深层性能问题**：

- **每段证明 ~90 分钟**（13 段跑了 20h+ 未完成）——**比 s=512 的每段 6.5 分钟慢 14 倍，与序列长度缩放完全相反**；
- **证明并行度异常低**：llama 证明期间仅 ~6 核满载（401-660%），对比 gemma-270m 同路径的 2400-3400%（24-34 核）；
- **数据正常性已排除**：layerwise CSV 显示 num_vars ≤24、最大张量 17M 元素，无病态张量；总 T_pad ≈17GB Fr，按 GPT-2 标定吞吐（61M 求值/s）理论证明时间应为 ~10 分钟量级——**实测慢 50×+**；
- **llama-7b 的其他成本**：context 生成 30 分钟 / 峰值 200GB 内存（839 节点图 + 13GB 权重 Fr 化 + 2²⁷ max_poly_len params，`--save-params` 可持久化复用）。

**影响**：经典路径同样受影响（与流式无关）——**这是 7B 级模型证明不可用的首要阻塞项**。**下一步**：对证明线程做 flamegraph（bench `--memory` 机制），定位 6 核瓶颈（候选：dp-crypto sumcheck 多 worker 的线程池配置、查表节点大表、LogUp GKR 实现的并行度）。

### 5.2 已交付的自适应段预算

`num_chunks_for_budget_with_max(budget, max_segments, model)`：有效段预算 = `max(budget, 流体积/max_segments)`，防止小预算在大模型上爆炸出数百段（llama-7b 512MB → 125 段 → 每段固定开销主导）。CLI：`--stream-max-segments`（默认 48）。API：`prove_streaming_with_max_segments`。

| 模型 | 序列 | 段数 | 融合(生成+证明) | 峰值内存 | 磁盘读 | verify |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| gpt2 | 128 | 3 | 197s | 28 GB | 0 | 1.5s |
| gpt2 | 512 | ~8 | 650s | 59 GB | 0 | 4.6s |
| gpt2 | 1024 | ~18 | 1343s | 84 GB | 0 | 5.6s |
| gemma-270m | 512 (prompt) | ~8 | 276s | 56 GB | 0 | 4.7s |
| gemma-270m | 1024 (prompt) | 16 | 4056s* | 193 GB | 0 | 5.7s |
| llama-2-7b | 512 (prompt) | **125** | 50953s* | 422 GB | 0 | 83.5s |

\* 含单线程前向（§4）；证明本体远小于总时长——**流式证明在 125 段规模下稳定运行 14 小时**：无死锁、无 OOM、内存平稳、**全程零磁盘读**、verify 通过（P1 偶发失败未在 125 段复现，支持"非确定性与段数无单调关系"）。

### 6.1 段数 × 每段固定开销的实测放大效应（llama-7b）

llama-7b T_pad≈63GB（Fr）÷ 512MB 预算 → **125 段**，实测每段 ~6.5 分钟（其中证明本体 ~1 分钟，其余为每段固定开销：表证明、边界承诺 ×2、opening、transcript）→ 总时长被固定开销主导至 14 小时。**直接结论**：

1. **表证明全局化（M2）**：125 次表证明是最大浪费项，收益量级 ~50%+；
2. **段预算必须按模型规模自适应**：7B 模型建议 2-4GB/段（→20-30 段，总时间预计缩至 2-3h）；512MB 预算只适合 ≤1B 参数模型；
3. 正面结果：流式机制（pin、流水线、种子链、合并）在 **125 段、422GB 内存、14 小时**压力下全程无故障——架构的规模稳定性得到验证。

---

## 复现命令

```bash
cd zkml
# 端到端（含生成+移位校验）
target/release/bench-llm --model gpt2 --hf openai-community/gpt2 --sequence 512 \
  --streaming --stream-budget-mb 512 --stream-window 2
# prompt 模式（跳过生成循环）
target/release/bench-llm --model gemma3 --hf google/gemma-3-270m-it --sequence 1024 \
  --streaming --stream-budget-mb 512 --stream-window 2 \
  --prompt-file model_cache/long_prompt.txt --bench model_cache/bench.prompt.csv
# verify 偶发失败压测
model_cache/stress_s1024.sh   # 3 轮 s=1024
```
