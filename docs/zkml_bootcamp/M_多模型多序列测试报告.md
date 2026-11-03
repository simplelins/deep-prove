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

模型可用性（逐文件 LFS magic 复核后更正）：真实权重 = **gpt2、gemma-3-270m-it、Llama-2-7b、Llama-3.2-1B（2.3GB BF16，此前误判为指针）**；LFS 指针（未拉取）= **gemma-3-1b-it**（1.86GB，gated）、TinyLlama、4 个 gguf 变体。原因：本机无 git-lfs，HF 下载走 git clone 流程时大文件只留指针。一键补拉脚本：`model_cache/download_missing_weights.sh`（内置各文件 sha256 校验，取自指针 oid；gemma-1b 为 gated 需 HF_TOKEN）。

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

## 5 复盘战役结果（同二进制全量重测，RUN_ID 20261103_090437）

| # | 测试 | 结果 | 关键数据 |
| --- | --- | --- | --- |
| gpt2_s128 | 冒烟 | ✅ PASS | verify 通过 |
| gpt2_s1024 | 最大可测 | ✅ PASS | |
| gpt2_s4096 | 架构拒绝 | ✅ 预期失败 | |
| gemma270m_s512 | | ✅ PASS | |
| gemma270m_s1024 | | ✅ PASS | |
| gemma270m_s4096 | 4k | ❌ FAIL（2h 后） | **MSM 越界 panic**（§5.1 P3） |
| gemma270m_s8192 | 8k | ❌ FAIL（5.8h 后） | 同 P3（分块分支越界） |
| gemma270m_s16384 | 16k | ❌ exit=137 | **OOM killed**（§5.2） |
| gemma1b_s512 | | ❌ exit=101 | **P1 复现**（chunk idx 6, Chunk(9)） |
| gemma1b_s1024 | | ❌ exit=101 | **P1 复现**（chunk idx 16, Chunk(17)） |
| llama7b_s512/s1024 | | 🔄 | 战役继续串行执行 |

### 5.1 P3（新）：PCS setup 容量不足——长序列 witness poly 越界

panic：`range end index 8388608 (2²³) out of range for slice of length 2097153 (2²¹+1)`，位置 `batch_poly_msm`（F1 补丁分块分支前后均有）。

- **g1_powers 长度 = 2²¹+1**（context 生成的 max_poly_len 仅 2²¹），而 s=4096 时 gemma-270m 的 witness poly 达 **2²³**（requant [4096, 2048]）——**原版代码同样越界**（`&g1_powers[..msm_size]`），非 F1 补丁引入；
- **疑点**：`num_chunks_for_polynomial` 的 chunking（threshold 2²⁰、MAX_CHUNKS=128）本应把大 poly 切成 ≤2²⁰ 块——但实测有 2²³ 的 poly 以单块进入 msm；**chunking 覆盖缺口待查**（哪个 poly 组装路径绕过了 `num_chunks_for_polynomial`）；
- **连带**：`MAX_CHUNKS=128` 上限使 >2²⁷ 的 poly（gemma-1b s=512 的 [512,262144]=2²⁷ 恰好 128 块；s=1024 的 2²⁸ 需要 256 块被 cap）每块超过 g1_powers 容量——**P1 偶发失败可能与 cap 后的切块尺寸/越界边缘直接相关**（此前仅表现为 sumcheck 失配的 P1 与越界 panic 可能同根）；
- **修复方向**：① 审计全部进入 `batch_commit` 的 poly 组装路径，统一走 `num_chunks_for_polynomial`；② `MAX_CHUNKS` 与 `g1_powers` 容量的一致性校验（context 生成时断言）；③ 短期绕过：campaign 对 s≥4096 显式调大 `CHUNK_SIZE_THRESHOLD`（RwLock 可调）并验证。

### 5.2 内存上限：s=16384 OOM（exit=137）

T_pad≈48GB Fr + 48 段窗口 + 段内 poly 工作集 → 峰值超 1TB RAM 被 SIGKILL。解法：① 降 window 至 1；② 降段预算不可行（段已受 P3 限制）；③ 根本解法 = P3 修复 + 评估分批验证。

### 5.3 P1 升级：跨模型复现（gemma-1b 2/2）

gemma-1b s=512（chunk idx 6, Chunk(9), 68 节点）与 s=1024（chunk idx 16, Chunk(17), 34 节点）均以 sumcheck 0th round 失配失败——**P1 从"偶发"升级为"1b 模型上必现"**。与 gemma-270m 的区别：1b 的 witness poly 更大（2²⁷，触及 MAX_CHUNKS 边界）——**P1 与 P3 可能同根**（chunking/容量边缘），合并排查。

## 6 性能数据汇总（流式 pipeline, window=2, budget=512MB）

### 5.1 llama-7b 证明性能异常（P2，待专项 profiling）

后续归因运行（llama-7b s=128，13 段）揭示了**独立于流式的深层性能问题**：

- **每段证明 ~90 分钟**（13 段跑了 20h+ 未完成）——**比 s=512 的每段 6.5 分钟慢 14 倍，与序列长度缩放完全相反**；
- **证明并行度异常低**：llama 证明期间仅 ~6 核满载（401-660%），对比 gemma-270m 同路径的 2400-3400%（24-34 核）；
- **数据正常性已排除**：layerwise CSV 显示 num_vars ≤24、最大张量 17M 元素，无病态张量；总 T_pad ≈17GB Fr，按 GPT-2 标定吞吐（61M 求值/s）理论证明时间应为 ~10 分钟量级——**实测慢 50×+**；
- **llama-7b 的其他成本**：context 生成 30 分钟 / 峰值 200GB 内存（839 节点图 + 13GB 权重 Fr 化 + 2²⁷ max_poly_len params，`--save-params` 可持久化复用）。

**影响**：经典路径同样受影响（与流式无关）——**这是 7B 级模型证明不可用的首要阻塞项**。**✅ 已定位（perf + dwarf 采样 + 线程级 utime 对比）**：**burn ndarray 后端的 i64 matmul 是朴素单线程实现**——

- perf 采样 97.8% CPU 在 `ndarray::general_mat_mul`，调用栈 `rayon scope → burn_ndarray::matmul → general_mat_mul`；
- 线程级对比：burn matmul 主计算线程 100% 满载，rayon 池其余线程各 ~15%，证明线程 60%——**有效并行 ≈2-3 核**；
- 根因：matrixmultiply gemm 只优化 f32/f64，**整数走朴素三重循环**，且 burn 仅对 batch 维并行（2D matmul 单线程）；
- llama-7b 的大 2D einsum（[512,32000]×[32000,4096] 等）每个都是分钟级单线程 → 839 节点前向累积成小时级，**前向主导了"每段证明时间"**（证明线程的 sumcheck/MSM 只占小头）；
- **与流式、与证明协议无关**——gemma-270m 快只是因为模型小、张量小。

**✅ 已修复（方案②落地）**：`WrappedTensor::matmul` 的 Rank2/Rank3 分支为 **i64 张量接入 rayon 并行 kernel**（`matmul_i64_parallel`：输出行分块 + 右操作数转置保 cache 连续；Rank3 支持 GQA 的 batch 广播），f32 路径不受影响（有优化 gemm）。正确性：朴素串行对拍单测 + 大值域无溢出单测 + llama 全链路 verify。

**实测（llama-7b s=128，13 段，端到端 verify 通过）**：

| 阶段 | 修复前 | 修复后 | 加速 |
| --- | --- | --- | --- |
| run loop forward | 20h+ 运行未完成 | **390s（6.5 分钟）** | ~20-30× |
| 段完成+证明投递 | ~90 分钟/段 × 13 | **1922s（32 分钟）总** | **~36×** |
| run loop 总计 | 20h+ | **2314s（38.6 分钟）** | **>30×** |
| verify | 未到达 | **69.3s 通过** | — |

streaming_prove_full 端到端 2891.6s（含 context 外的 quant 336s）；内存峰值 366GB；磁盘读 0。

其余解法仍可叠加：① CUDA 后端跑前向（`--features cuda`）；③ 量化域降宽（i32/i16，协议改动大）。

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
| llama-2-7b | 512 (prompt, **修复后**) | **48** | **6752s*** | 420 GB | 0 | 83.5s |

**修复前后对照（llama-7b s=512）**：rayon i64 matmul + 自适应段预算（125→48 段）后，端到端 **14.15h → 1.88h（7.5×）**；分项打点 forward=824s（13.7min）、dispatch=5520s（92min）、finish=396s；verify 48 段全过；磁盘读 0。剩余大头是段证明本体（dispatch 92min ≈ 段固定开销 × 48）——表证明全局化（M2）仍是下一个优化杠杆。

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
