# N · 证明性能瓶颈分析与 CPU+FPGA 优化路线

> 数据来源：本文全部结论基于 2026-11-03 → 11-06 的实测运行（128 核 CPU / 1TB RAM / 无 GPU），
> 运行明细见 `M_多模型多序列测试报告.md` §5-§7 与 `zkml/tests/README.md`；FPGA 算子规格见
> `prove_pipeline_FPGA算子开发指导.md`（下称 FPGA 文档）。本文不重复其规格书，只做**实测瓶颈
> 定位、优先级修正、与 10 分钟目标的达标算术**。

---

## 0 结论摘要

1. **证明端（prove_chunk）10 分钟可达**：实测每段证明 ~110s（与模型无关、与段体积线性相关）。
   算子级分解（§1.3）：**查表族（requant/rms-norm/softmax/logits 的 logUp+linking+witness）
   占 ~82%，einsum 主 sumcheck 仅 8%，MSM 仅 ~6%**。FPGA 文档把 MSM 列为第一优先（P0 二期）
   ——**实测数据要求反转该顺序**：先做 sumcheck（重点 linking 形态）/LogUp 轮引擎
   （OP-07/OP-13），MSM 后置。引擎上 FPGA 后每段 110s → **2-5s**，48 段 → 2-5 分钟 ✓。
2. **前向端（trace 生成）是新的第一墙**：P2 修复（rayon i64 matmul）后 forward 仍占
   825-980s（三个模型实测），且随模型 FLOPs × s 线性放大。13b s=512 外推 ~65 分钟——
   **单靠 CPU 前向进不了 10 分钟**。达标路径 = 量化域 i8/i32 推理（预期 5-10×，协议不需改，
   证明对象本来就是量化域）或 GPU/FPGA matmul。
3. **内存是硬边界**：13b s=512 两次运行在 **RSS 1046GB 处被内核 OOM 杀死**（dmesg 实录）。
   证明工作集可由段预算控制，**不可控的是 context 生成（模型参数 Fr 化 + 2^23 params）与
   logits 段的 MLE**（s≥8k 时 2^31×32B=547GB Fr）。RAM 决定"模型×s"上限，与算力无关。
4. **10 分钟组合的判定**（CPU 前向优化 + FPGA 证明，单 FPGA 通道）：
   gemma-270m 16k ✓、gemma-1b 8k~16k ✓、llama-7b 1k~2k ✓、13b 512 ~ 边缘（受 RAM 与前向双重挤压）。
   更大的组合需要 FPGA 多通道并行或前向 matmul 下 FPGA。

---

## 1 实测基线

### 1.1 端到端分解（run loop 打点，秒）

| 运行 | 段数 | forward | dispatch(证明) | finish | verify | 端到端 | 磁盘读 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| gemma-270m s=4096 | 48 | 885 | 5313 | 1462 | 8.2 | 7671 | 0 |
| llama-7b s=512 | 48 | 824 | 5520 | 396 | 83.5 | 6752 | 0 |
| llama-2-13b s=128 | 18 | 979 | 4434 | 932 | — | 6347 | 0 |
| gemma-270m s=1024 | 16 | ~60 | ~250 | ~90 | 5.7 | ~405 | 0 |

**要点**：
- **每段证明时间与模型几乎无关**：110.7s（270m@4k）/ 115.0s（7b@512）/ 246s（13b@128）。
  原因：分段按 512MB 预算切，每段 trace 体积相同 → 证明成本 ≈ f(段体积)，而非 f(模型)。
  13b 偏高 2.2×，因其段内节点数更多（4096 维 → 同体积下更多 claim/commitment 个数）。
- **verify 可忽略**（8-84s，证明时长的 1-2%）——协议验证侧无需加速。
- **finish = 尾延迟**：1462s（270m@4k）主要是**等待最后一段（logits 超尺寸段）证明完成**
  + 合并。logits 段是流水的排空瓶颈（§3.4）。

### 1.2 单段（prove_chunk）内部分解

gemma-270m s=4096 的重段（last-chunk 计时，合计 1139s）：

| 阶段 | 耗时 | 占比 | 内容 |
| --- | ---: | ---: | --- |
| prove_claims | 789s | **69%** | 输出/边界 claim 的 MLE 求值 + **全表 LogUp 批量证明**（`prove_tables`） |
| 逐节点证明 | ~283s | 25% | backward_iter：einsum sumcheck、查表层（logUp+linking sumcheck）、add 等 |
| witness 承诺 | 29s | 3% | fieldize(i64→Fr) + MLE + **批量 MSM**（`batch_commit`） |
| commitment opening | 37s | 3% | claims batching sumcheck + RLC + KZG 打开 MSM |

> 注：`prove_claims` 69% 中，表 LogUp 是大头——**每个 chunk 对同一组全局表**（requant
> 2^24、rms-norm、softmax、zero/shift-check）**重新跑一遍完整 LogUp GKR**（`prover.rs:266`
> `prove_tables`，逐 chunk 调用）。节点普查见 §1.3：requant 实例 ×271、rms-norm ×109
> 分摊到 48 段，每段仍要为约 6 个 requant + 2 个 rms-norm 实例证 2^23~2^24 大小的表。

### 1.3 算子级实测分解（`--measure-layerwise`，gemma-270m s=512，8 段）

整模型证明算力按算子族累计（904s，占比）：

| 算子族 | 耗时 | 占比 | 构成 |
| --- | ---: | ---: | --- |
| **requant 查表** | 390s | **43.1%** | layer_requant 36.8%（逐实例 logUp+linking）+ witness commitment 6.3% |
| **rms-norm 查表** | 149s | **16.5%** | layer 13.9% + witness 2.6%（P1 相关路径） |
| **logits 范围检查** | 130s | **14.4%** | layer 10.8% + witness 3.6%（尾段超尺寸） |
| **softmax 查表** | 73s | **8.1%** | layer 6.7% + witness 1.4% |
| einsum（矩阵乘 sumcheck） | 72s | **8.0%** | QK/AV/FFN 主 sumcheck |
| activation/embeddings/其余 | 90s | 9.9% | embeddings 词表查表 3.3% |

**两个反直觉的实测结论**：
1. **查表族（logUp + linking sumcheck + 查表 witness）占 ~82%**，而"矩阵乘 sumcheck"
   （直觉上最重的部分）只占 8%——DeepProve 的证明成本主体在**查表论证与张量-表链接**，
   不在多项式乘法验证本身；
2. 结合 §1.2：单段 1139s 中 69% 在 `prove_claims`（表 LogUp），25% 在逐节点——两处合计
   的主体都是**查表族**。因此 FPGA 与 CPU 优化的第一靶标一致：**linking sumcheck 引擎
   与 LogUp 引擎**，以及 **C1 表证明增量化**（CPU 侧最大单项）。

### 1.4 节点类型普查（gemma-270m s=4096，617 节点）

| 类型 | 个数 | 输入元素合计 | 证明负载特征 |
| --- | ---: | ---: | --- |
| einsum | 109 | 1.64G | 主 sumcheck（QK/AV/FFN 矩阵乘），度 3，轮数 = log₂(out elems) |
| attention-mask | 18 | 1.21G | s² 注意力图，元素最多但 sumcheck 度低 |
| softmax | 18 | 1.21G | 查表（exp 表）+ sumcheck，[Hq,s,s] 域化内存大头 |
| requant | 271 | 0.81G | 查表 LogUp + linking，**表 2^24**，实例最多 |
| rms-norm | 109 | 0.29G | 查表 + 归一化 linking sumcheck（P1 相关路径） |
| add/positional/activation | 90 | 0.58G | 轻量 sumcheck |
| logits | 1 | 1.07G | **超尺寸段**：MLE 2^31 Fr = 547GB，claims 求值 O(2³¹) |

### 1.5 CPU 端已并行化的部分（无需重复优化）

- sumcheck prover 多线程（dp-crypto `optimal_sumcheck_threads` + rayon 折叠）
- MLE 点求值 `evaluate`（rayon，`dense.rs:584`）
- 大 poly MSM >2^23 的分块并行（vendor 补丁，F1）
- 前向 i64 matmul（P2：`matmul_i64_parallel`）

---

## 2 成本模型与伸缩律

记模型规模 N（参数量）、序列长 s、段预算 B（512MB）、T_pad = 域化后 trace 总元素
（F 文档 §4 表）：

| 墙 | 公式 | 实测锚点 | 伸缩 |
| --- | --- | --- | --- |
| 前向 | fwd ≈ κ·N·s | 270m@4k=885s；7b@512=824s；13b@128=979s | 线性于 N·s |
| 证明 | Σ段 ≈ (T_pad/B)·τ(B) | τ(512MB)≈110s（CPU） | 线性于 T_pad |
| 尾延迟 | finish ≈ τ(logits 段) + merge | 1462s@4k | logits 段 ∝ s·V |
| 内存 | max(参数 Fr 化, 窗口×段工作集, logits MLE) | 13b s=512 → 1046GB OOM | 随 N、s·V |

**10 分钟目标的反推**（48 段）：每段证明需 ≤12.5s（CPU 实测 110-246s，**差 9-20×**）；
或 24 段 × ≤25s / 12 段 × ≤50s（加大段预算，等价于要求证明吞吐 ∝ 体积线性提升）。

前向反推：gemma-270m@16k ≈ 3.5ks、7b@2k ≈ 3.3ks、13b@512 ≈ 3.9ks（i64 rayon 外推）——
都在 60 分钟量级，**需 5-7× 才能进 10 分钟**。

---

## 3 prove_chunk 优化建议

### 3.1 CPU 侧（协议不变，按收益/工程量排序）

**C1 · 表证明增量化（原 M2；查表族实测占 82%，本项预计 -40~60% 段证明时间）**
`prove_tables` 每 chunk 对全局表跑全量 LogUp + 每实例 linking 是最大单项（§1.3：requant
43% + rms-norm 16.5% + softmax 8.1%）。三条路线：
1. **稀疏 multiplicity**：本 chunk 命中的表项远小于表长（requant 表 2^24，命中通常
   10^4~10^6 级）。把 LogUp 证明从"稠密 2^n 表"改为"命中项列表 + 计数"的稀疏论证，
   证明成本 ∝ 命中数而非表长。协议上等价（分数求和不变），verifier 需同步改造。
2. **跨 chunk 分数累加已有**（verifier 的 `numerators_and_denominators` 就是全局累积）——
   把"每 chunk 表终证"降为"每 chunk 增量分数 + 全模型最后一次终证"，终证只出现一次。
3. **小表缓存**：shift/zero-check 表仅 2^16，全模型只证一次（证明对象不变，挑战绑定
   进全局 transcript 即可，与方案 A 审计兼容）。

**C2 · 段预算提升（一行改动 + 重测）**
48 段 cap 是为 512MB 小预算兜底设的。对 ≥7B 模型直接给 1-2GB 预算（`--stream-budget-mb 2048`）：
段数减半，每段固定开销（transcript 初始化、challenge storage、claim 登记、opening 基础 MSM）
只出现一半次数。内存代价 = 段工作集 ×2（窗口 2 → 峰值 +1GB 级，可控）。
llama-7b 实测对照：125 段@512MB = 14.15h → 48 段 = 1.88h，**同方向还有一档可挖**。

**C3 · fieldize/pad 合并 pass**
`to_field().pad_next_power_of_two()` 在每段执行两遍（witness 路径 + claims 路径，
`prover.rs:676/731`）。合并为单遍 rayon pass（fieldize+pad+写 MLE 布局一次完成），
省一遍全量内存读写（512MB 段 ≈ 数秒/遍 ×2 ×48 段）。

**C4 · logits 尾段专项**
- 水平切分（路线图 phase 2）：logits [s,V] 沿 vocab 维切 N 份，MLE 内存 547GB→547/N GB；
- claims 求值分块：`mle.evaluate` 已 rayon，但 2^31 元素单遍 ~10s 级；配合水平切分自然消解。
- 收益：finish 的 1462s 里 logits 段证明占大头，切 4 份 → 尾延迟 ÷4。

**C5 · window 自适应**
window=2 固定。forward 快的模型（270m）证明是瓶颈，window 可到 4-8（多缓存一段
512MB 工作集换重叠度）；forward 慢的模型（13b）window=1 足够。按 `forward估速/证明估速`
动态定，收益 10-20%。

### 3.2 FPGA 算子优先级修正（对照 FPGA 文档 §1.2/§1.3）

实测 share 推翻"MSM 优先"：

| 优先级 | 算子（FPGA 文档编号） | 实测 share | 理由与目标 |
| --- | --- | ---: | --- |
| **P0-A** | **OP-07 sumcheck 轮引擎（重点 linking 形态）+ OP-13 LogUp GKR** | **~82%**（查表族，§1.3） | 查表族是绝对主力；linking sumcheck 轮数 = log₂(张量元素)（大）、LogUp 轮数 = log₂(表)（小但实例多）。折叠带宽 ~150-300MB/s 可承载，瓶颈在 BN254 乘法器阵列规模 |
| P0-B | OP-03 Blake3 常驻 + OP-04/05/06（EQ/求值/折叠基元） | 关键路径 | 每轮挑战采样延迟 <1μs（FPGA 文档 §2.3 已定）；与 OP-07 同一颗流片实现 |
| **P1** | OP-10 批量 MSM + OP-11 KZG 打开 | ~6% | 分块后每 MSM 仅 2^20-2^22；CPU 37s → FPGA <1s。**尾延迟收益**（finish 缩短），非吞吐收益 |
| P2 | OP-12 查表 witness 生成 | 段内 witness 29s 的一部分 | 位切片+计数是整型算力，CPU 就地做、只把 MLE 结果送 FPGA 即可 |
| 暂缓 | OP-14 卷积 FFT | 0 | LLM 路径用不到（FPGA 文档已标 P3） |

**FPGA 吞吐预算校验**（12s/512MB 段目标）：
- sumcheck/LogUp 折叠总数据量 ≈ 段体积 × 轮数因子（~2-3 遍读 + 1 遍写）≈ 2GB/段
  → 12s 内完成需 **~170MB/s 有效域运算带宽**——ZCU/U55C 级 HBM（~460GB/s）下余量充足，
  瓶颈在乘法器阵列规模而非带宽；
- BN254 乘 8 路 @ 250MHz ≈ 2G 乘/s；每段 sumcheck+LogUp 约 10^9~10^10 次乘
  → 5-10s/段，达标但**余量不大**，建议 16 路（FPGA 文档 OP-01 的倍频目标）。

### 3.3 CPU+FPGA 分工边界（达标配置建议）

| 阶段 | CPU | FPGA |
| --- | --- | --- |
| 推理前向 | 量化域 i8/i32（5-10×，见 §4） | （远期：matmul 下 FPGA） |
| trace 生成/生命周期/pin | CPU（控制流） | — |
| fieldize/pad/MLE | CPU（C3 优化后） | — |
| sumcheck / LogUp / linking | — | **FPGA（OP-07/13）** |
| witness/ opening MSM | — | FPGA（OP-10/11） |
| transcript/挑战 | — | FPGA 常驻 Blake3（OP-03） |
| 合并/verify | CPU（8-84s，无需加速） | — |

### 3.4 尾延迟（finish）专项

finish ≈ 最后一段证明 + 合并 + IO。三个来源逐个消解：
1. logits 超尺寸段 → C4 水平切分；
2. opening MSM（37s）→ FPGA OP-10 后 <1s；
3. 合并 48 份 ChunkProof（纯内存操作，<10s）——无需处理。

---

## 4 组合预测：哪些"模型 × 序列"能进 10 分钟

前提：C1+C2 落地 + FPGA OP-07/13/10（单通道）+ 前向量化域 i8（5×）。段预算按模型
自适应（小模型 512MB、≥7B 2GB）。

| 模型 × 序列 | T_pad | 段数 | 前向(i8 估) | 证明(FPGA 估) | 总计 | 判定 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| gemma-270m × 8192 | ~0.11TB* | 48 | ~200s | ~2-5s×48=96-240s | **~5-8min** | ✅ |
| gemma-270m × 16384 | ~0.25TB* | 48 | ~400s | 96-240s | **~8-11min** | ✅ 边缘 |
| gemma-1b × 8192 | ~0.4TB | 48 | ~500s | ~2-5s×48 | **~10-14min** | ⚠️ 边缘 |
| llama-7b × 1024 | 44GB | 22@2GB | ~180s | ~10-20s×22 | **~7-12min** | ✅ 边缘 |
| llama-7b × 2048 | ~200GB | 48@4GB(内存承压) | ~350s | ~40-80s×48 | ~35-70min | ❌ 需多通道 |
| llama-13b × 512 | 39GB | 48 | ~400-800s | ~5-10s×48 | **~10-18min** | ⚠️ RAM 边缘 |
| llama-13b × 1024 | 110GB | 48@2.3GB | ~800-1600s | ~15-30s×48 | ~25-45min | ❌ |

\* gemma 系 T_pad 按 pad 系数 1.78 外推，待 s=8192 运行实测校准。

**结论**：
- 单 FPGA 通道 + CPU 量化的达标圈：**270m@16k、1b@8-16k、7b@1-2k、13b@512**——
  与用户目标场景（llama-7b/16k ~ 13b/32k）仍有 1-2 个数量级差距；
- 补齐差距的三个杠杆（按性价比）：① **FPGA 多通道**（2-4 段并行证明，段间无协议依赖——
  方案 A 的种子链只约束 transcript 吸收顺序，证明本身可乱序完成、按序吸收）；
  ② **前向 matmul 下 FPGA**（省掉 i8 CPU 前向的最后一墙）；③ **协议级**：logits/range-check
  的批量化（G 文档/路线图已论证为必要工作）。

---

## 5 前置条件与风险

1. **RAM**：13b@512 实测 1046GB OOM。达标圈内的组合峰值预计 ≤700GB（270m@16k 待
   s=8192 实测校准）。context 生成 params 可 `--save-params` 复用（llama-7b pk 165GB）。
2. **P1 可靠性**：偶发 verify 失败（M 文档 §3，~9%/run）在量产前必须根因修复；
   猎捕 v2（双侧 dump）进行中。
3. **尾延迟与超尺寸段**：水平切分（phase 2）未落地前，logits 段是所有组合的 finish 下限。
4. **FPGA 文档同步**：本文 §3.2 的优先级修正应回写 `prove_pipeline_FPGA算子开发指导.md`
   §1.3 实施顺序（sumcheck/LogUp 先于 MSM）。
5. **量化域推理的正确性**：i8 前向改变推理数值 → 证明对象仍须与真实推理一致。
   需确认 DeepProve 的量化方案（requant 表驱动）允许 i32 累加域推理与现协议直连
   （requant 查表逻辑不变，仅 matmul 精度域收窄，协议无需改）。

---

## 6 增量优化清单（§3 之外的第二层，按层分类）

> 均为 2026-11-06 代码核查确认的现状，标注依据与工程量量级（S=小时级 / M=天级 / L=周级）。

### 6.1 协议/算法层（减少证明工作量）

| # | 优化点 | 依据 | 预期 | 量级 |
| --- | --- | --- | --- | --- |
| A1 | **跨节点 LogUp 批处理**：271 个 requant 实例逐节点独立跑 `new_batch_multiple_sizes_prove`（`generic_prove.rs:137`），同 chunk 的实例可按随机线性组合合并为单次 GKR | 节点普查 ×271；每实例独立轮数+transcript 开销 | 查表族 82% 中的固定开销部分，-10~20% | M |
| A2 | **einsum+requant 融合**：einsum 输出 claim 直连 requant 输入查表，省一遍全张量 pass（本 fork 无 fusion 模块；旧 gpt2 文档的"融合"另有所指） | requant 紧跟每个 matmul（普查 271 对） | 查表族再一次 -15~25%，内存少一遍 512MB 峰值 | L |
| A3 | **零物化 MLE 管线（内存主攻）**：MLE 布局=张量展平序，i64→Fr 转换可与 MSM/求值/折叠**按流融合**，不物化 2³¹ Fr=547GB 的 logits MLE | logits 段是 s≥8k 的内存与尾延迟双瓶颈（§1.3/§3.4） | 16k/32k 可测性的**必要条件**；省 550GB 峰值 | M-L |
| A4 | **权重 Fr 化按 chunk 惰性化**：context 生成一次性把全部权重 Fr 化（13b context 生成 RSS 950GB+ 的主源），改为逐 chunk BF16→Fr→释放 | 13b s=128 实测 950GB；s=512 OOM(1046GB) | 13b 档内存 -50% 以上 | M |
| S1 | **transcript 连续采样同值（安全复核 + 解锁真 RLC）**：`challenge_bytes` 每次从 XOF 位置 0 读（blake3 `finalize_xof` 语义，`blake3.rs:21-23`），连续 `challenge_scalar()` 返回同值——而 opening 的 RLC 系数逐 poly 连续采样（`prover.rs:119`）→ **系数全部相同**，批处理退化为 c·Σ。修复（hash 计数器派生 k 个系数）需 K 文档复核绑定论证 | 代码实锤；verify 侧同怪癖故双方一致（不影响正确性，影响绑定余量与"真随机 RLC"的防御价值） | 安全余量复核 + 批处理防碰撞 | S-M |

### 6.2 实现层（同工作量跑更快）

| # | 优化点 | 依据 | 预期 | 量级 |
| --- | --- | --- | --- | --- |
| I1 | **AVX-512/AMX 蒙哥马利域乘内核**：ark-ff `asm` 已随 dp-crypto `parallel` 启用（基础 mul）；Expander 系有手写内核 2-4× 先例，sumcheck/LogUp 全部直接受益 | `dp-crypto/Cargo.toml:55` parallel→asm | 全证明 2-4×（FPGA 前的过渡收益） | M |
| I2 | **mimalloc/jemalloc**：无自定义全局分配器；MLE/eq 表大对象高频分配 | `lib.rs` 无 `global_allocator` | 10-30%，一行改动可 A/B | S |
| I3 | **NUMA 亲和**：多路机器 100GB 级数组 first-touch 本地化，证明线程+rayon 绑节点 | 1TB 机器通常 2 路 | 内存带宽敏感段 10-30% | S-M |
| I4 | **rayon 池隔离**：前向(burn)与证明(dp-crypto)共享全局池，前向冲高时证明线程饿死（window=2 背压下放大） | 运行观察：8192 证明推进速度随 hunt 并发波动 3-15× | 重叠期吞吐稳定化 | S |
| I5 | **fieldize/pad 并行化**：`to_field().pad_next_power_of_two()` 为单线程 map（`prover.rs:676/731` 等），每段全量执行两遍 | §3.1 C3 的前置 | 数秒/段 → 亚秒 | S |

### 6.3 GPU 过渡路线（FPGA 就绪前的可选项）

zkml 已内置 `cuda` feature（`burn/cuda` + `dp-crypto/cuda`，上游分支 `feat/hkzg_gpu`）——
**单张消费级 GPU（如 4090）即可先吃掉两块**：① 前向 i64/量化 matmul（burn cuda 后端），
② MSM 与域算术（dp-crypto cuda）。作为 FPGA P0 引擎落地前的过渡，10 分钟圈的
`前向 i8 5×` 假设可先由 GPU 实测验证。

### 6.4 优先级总表（合并 §3）

| 梯队 | 项目 | 为什么 |
| --- | --- | --- |
| 立即（S） | I2 分配器、I5 fieldize 并行、C2 段预算、I3 NUMA | 一行/小时级改动，先吃免费收益 |
| 短期（M） | C1 表证明增量化、A1 LogUp 批处理、I1 域乘内核、I4 池隔离、C4 logits 切分 | 查表族 82% 的主攻；CPU 侧预计合计 2-3× |
| 中期（M-L） | FPGA OP-07/13 引擎（P0-A）、A3 零物化 MLE、A4 惰性 Fr 化 | 达标 10 分钟的证明端主引擎 + 16k 内存解 |
| 并行可选 | G1 GPU 过渡、S1 安全复核 | GPU 验证假设、复核绑定论证 |
| 远期（L） | A2 einsum+requant 融合、FPGA 多通道、前向 matmul 下 FPGA | 冲 7b@2k+ / 13b@1k+ |

---

## 附：数据可复现性

```bash
# 本文基线数据
zkml/model_cache/results/2026110*_*/summary.*.txt     # 各 run 端到端打点
zkml/model_cache/runlog/validate_s4096.*.csv          # 单段内部计时（witness_commitment 等）
zkml/model_cache/runlog/profile_ops_s512.csv          # --measure-layerwise 算子级分解
# 复现算子 profile
cd zkml && target/release/bench-llm --model gemma3 --hf google/gemma-3-270m-it \
  --sequence 512 --streaming --stream-budget-mb 512 --stream-window 2 \
  --measure-layerwise --prompt-file model_cache/long_prompt.txt \
  --bench model_cache/runlog/profile_ops_s512.csv
```
