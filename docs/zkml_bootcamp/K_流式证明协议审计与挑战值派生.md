# 流式证明协议审计与挑战值派生设计

## 文档控制

| 项目 | 内容 |
| --- | --- |
| 文档编号 | K |
| 所属系列 | zkml_bootcamp |
| 版本 | 1.2（S1：transcript 连续采样同值修复，见 §补遗） |
| 发布日期 | 2026-10-30 |
| 文档状态 | 已发布 |
| 适用代码基线 | `zkml/src/iop/prover.rs`、`zkml/src/iop/verifier.rs`、`zkml/src/iop/chunking.rs`、`zkml/src/iop/mod.rs`、`zkml/src/lookup/context.rs`、`zkml/src/model/context.rs`、dp-crypto `sumcheck/prover.rs`（commit 22e8e93） |

### 修订记录

| 版本 | 日期 | 修订说明 |
| --- | --- | --- |
| 1.0 | 2026-10-30 | 首次发布：prove/verify 两侧挑战值派生点与吸收顺序审计、安全强度定量论证（ε 预算 + grinding）、流式分段影响分析、方案 A（全局串行 transcript）设计与实施要求 |
| 1.1 | 2026-10-30 | 补充 §5.5 方案 A 实现记录：全局 transcript 域分离字符串、输入绑定/段数绑定/proven chunk 吸收的具体函数、吸收顺序 = 段完成顺序（与最终 Proof 的 Vec 顺序一致，非 chunk_id 序）、流式与经典派生的验证入口分离、负向测试 |

---

## 1 引言

### 1.1 目的

流式分段证明（trace 按段生成→内存证明→合并，见 J 文档）改变了证明的执行时序，但不改变协议结构。本文以 DeepProve 论文的安全框架为基准，回答三个问题：

1. 当前实现（含 chunked 证明）的每个随机性（挑战值）是否满足 Fiat-Shamir 变换的 soundness 前提（commit-then-challenge）；
2. 分段证明的安全强度定量上是多少，是否满足要求；
3. 流式分段需要（或应当）做哪些协议层面的补充，即方案 A（全局串行 transcript）的设计依据。

### 1.2 结论速览

| 结论 | 内容 |
| --- | --- |
| C1 | 当前 per-chunk 独立证明在协议层面 **sound**：每个 chunk 内所有挑战值均在该 chunk 的见证承诺吸收进 transcript 之后派生；verifier 侧重放顺序一致 |
| C2 | IO 锚定完整：证明绑定"这条输入 → 这条输出"（verifier 用真实 IO 在 FS 点重算对比） |
| C3 | BN254/HyperKZG 实例具体安全强度 ≈ 100 bit（PCS 假设主导）；FS/sumcheck 项 ≲ 2^-233。**生产必须固定 BN254/HyperKZG**，Goldilocks/BaseFold 在分块场景 grinding 余量不足 |
| C4 | 分段的固有代价是 grinding 面积从全链缩小到单段（每次尝试代价 = 单段证明成本），BN254 下无实际影响；段数 N 的 union bound 代价可忽略 |
| C5 | 方案 A（全局串行 transcript）是**严格增强而非 soundness 必要条件**：修复 W1（种子绑定运行实例）、W2（段间随机性链）、W3（段数绑定），与 FPGA 流水线兼容 |
| C6 | 已知缺口：`check_model_chunking` 对 replaced-nodes 周边边的检查存在 TODO（model/context.rs:700，低危）；方案为非 ZK；证明对象为量化域推理 |

---

## 2 协议安全框架（论文基准）

### 2.1 逐算子 round-by-round 知识可靠性

论文附录对每个算子 PIOP 给出 ε_rbr（round-by-round knowledge soundness）定理：

| 协议 | ε_rbr 上界 | 论文出处 |
| --- | --- | --- |
| sumcheck | O(polylog(n) / \|F\|) | Theorem 1/2 |
| LogUp 查表 | O(max(h, log n) / \|F\|) | Theorem 5 |
| 批量矩阵乘 | max(h·(1 + 2·log max(n_i)·log max(m_i)) / \|F\|, ε_sum) | Theorem 6 |
| Requant / Embedding / RoPE / argmax / RMSNorm 等 | max(ε_lkp, ε_sum, \|F\|⁻¹) | Theorem 7–12 |

所有上界均为 polylog/\|F\| 形态——安全性由域大小主导。

### 2.2 非交互化与分块

论文的两个关键声明：

1. **BCS 变换**（论文 §4.2 末）：*"we … include formal proofs for the round-by-round (knowledge) soundness of all our protocols which, enables us to show that our protocol is knowledge sound when making it non-interactive using the BCS transformation [BCS16]"*。BCS/FS 安全的前提：**每个挑战点在所有相关承诺被吸收进 transcript 之后派生**。
2. **输出 claim 派生**（论文 §4.2）：*"This claim is actually derived from the Fiat-Shamir transformation after all commitments and auxiliary information have been correctly integrated into the transcript."*
3. **分块证明**（论文 §4.5）：*"By committing to inputs/outputs of each chunk, proving of each chunk becomes independent from all the other chunks"*。安全性绑定 = ①划分与原模型 DAG 一致（不漏节点/边）；②每个 chunk 的输入承诺 = 相邻 chunk 对应输出的承诺。**段间独立是论文的设计意图，流式分段与 §4.5 协议结构同构。**

### 2.3 PCS 前提

- Binding：对同一承诺 C 找到两个不同多项式均能通过 Open 的概率可忽略（论文附录 A Definition/Theorem，Eval 为 argument of knowledge，存在 extractor）。
- HyperKZG：BN254 上的 q-type / 离散对数类假设，具体强度 ≈ 100–120 bit（BN254 配对安全级）。
- BaseFold：透明（plausible post-quantum secure），但域为 Goldilocks（64-bit），见 §4.3 的强度分析。

---

## 3 实现审计：挑战值派生点与吸收顺序

### 3.1 全局层

| 步骤 | 位置 | 动作 |
| --- | --- | --- |
| G1 | prover.rs:583-585 | 全局 transcript：`T::new(b"model_proving")` + `ctx.write_to_transcript`（吸收 PCS 参数与模型权重承诺，ProverContext::write_to_transcript → commitment_ctx） |
| G2 | prover_graph.rs:170-172 | 从全局 transcript squeeze **唯一公共挑战** `challenge` |
| G3 | chunking.rs:2213-2219 | 每个 chunk 的独立 transcript：`T::new(b"model_chunk_proving")` + `append_scalars(&[challenge])`（域分离字符串不同，防跨协议复用） |

要点：`challenge` 只由 **setup**（公开上下文）派生，与本次执行的 witness/IO 无关。它仅是域分离种子；所有真正消耗 soundness 的随机点都在 chunk 内 post-commitment 派生（见 3.2）。verifier 侧完全对称（verifier.rs:597 → 681 → 684）。

### 3.2 chunk 内（prover 侧，prove_chunk prover.rs:588-919）

按执行顺序：

| # | 步骤 | 位置 | 吸收进 transcript 的内容 | 派生的随机性 |
| --- | --- | --- | --- | --- |
| 1 | chunk 身份绑定 | prover.rs:600 → chunking.rs:511-550 | chunk_id、出入边界边、模型 IO 边 | — |
| 2 | 见证承诺 | prover.rs:607 → lookup/context.rs:224-498 | 见证多项式承诺（445-457）、复数度承诺（459-461）、chunk IO（边界）承诺（490-491）；承诺由 batch_commit（MSM）产生 | — |
| 3 | lookup 挑战 | prover.rs:616-620 | — | ChallengeStorage::initialise（**步骤 2 之后**）✓ |
| 4 | 模型输出 claim | prover.rs:629-667 | — | `compute_claim`：从 transcript squeeze 求值点（**步骤 2 之后**）✓ |
| 5 | 出边 claims / common_point | prover.rs:673-712 | — | `read_challenges` 逐变量补齐公共点（**步骤 2 之后**）✓ |
| 6 | 逐算子 sumcheck / LogUp | prover.rs:717-856；dp-crypto sumcheck/prover.rs:200-207 | 每轮 sumcheck 消息 | **逐轮 FS**：`append_and_sample(b"Internal round")` ✓ |
| 7 | 边界 witness claims | prover.rs:862-870 | — | 承诺 id = SHA256(node, port)（确定性，非随机性） |
| 8 | 表证明 | prover.rs:874-876 | — | lookup 挑战（步骤 3 已派生） |
| 9 | PCS 批量 opening | prover.rs:884-890 | — | opening 点由 transcript 派生（**全部承诺与证明之后**）✓ |

### 3.3 chunk 内（verifier 侧重放，verify_chunk verifier.rs:146-690）

重放顺序与 prover 逐项对应：chunk 数据（155）→ 各节点 LayerProof（含该节点承诺，207）→ 表证明承诺（210-212）→ chunk IO 承诺（214-216）→ ChallengeStorage（218-224）→ 输出 claim 重算（226-282）→ common_point（284-305）→ 反向逐节点验证（311-493）→ opening 验证。**顺序一致 ⇒ verifier 派生出与 prover 相同的挑战值 ⇒ FS 重放成立。**

### 3.4 IO 锚定（证明绑定"这条输入→这条输出"）

| 锚点 | 位置 | 机制 |
| --- | --- | --- |
| 输出 | verifier.rs:226-282 | verifier 取真实 `io.output`，在 FS 派生点（post-commitment）手工求值 MLE，与证明链的输出 claim 对比 |
| 输入 | verifier.rs:427-492 | 反向链到达 Input 节点时，verifier 取真实 `io.input` 在输入 claim 点求值，`verify_input_claim` 对比 |
| 权重 | setup 承诺 + opening | 模型权重多项式在预处理期承诺，逐层 claim 经批量 opening 校验，不可替换 |

### 3.5 分块绑定检查

| 检查 | 位置 | 防御的攻击 |
| --- | --- | --- |
| 节点划分集合 == 模型图节点集合（含 replaced_nodes 展开），内节点不重复 | model/context.rs:599-661 | 漏算某层 / 重复证明某层 |
| 边接线束检查 | model/context.rs:698（check_model_graph_edges_from_chunks） | 篡改数据流 wiring |
| 边界边分组一致性（重建比对） | model/context.rs:705-717 | 虚假边界结构 |
| 跨 chunk 承诺相等 | chunking.rs:634-711（check_chunk_commitment_consistency），verifier.rs:670-675 | 相邻段使用不同张量值（靠 PCS binding 归约到困难问题） |

**已知缺口（C6）**：model/context.rs:700 `ToDo: check edges involving replaced nodes`——水平切分（einsum 行切）产生的 replaced 节点周边边未做逐边比对。缓解：这些边的张量值仍被两段的 boundary 承诺覆盖，篡改需破坏 PCS binding；但作为纵深防御，阶段 5 实现水平切分流式支持时须补齐该检查。

---

## 4 安全强度定量论证

### 4.1 ε 预算表（BN254 Fr，|F| ≈ 2^254）

以批量矩阵乘上界（Theorem 6）为保守代表，取 log n·log m ≈ 40² = 1600（对应 2^20 元素级张量），h ≤ 段内批量实例数：

| 项 | 误差 | 说明 |
| --- | --- | --- |
| 单层 ε_rbr | ≈ h·1601/2^254 ≈ **2^-242**（h≈2 时） | 最坏单层 |
| 单段全链（~22 层，GPT-2 block） | union bound ≈ **2^-236** | Σ 单层 |
| 全模型（~260 层，1 段） | ≈ **2^-233** | Σ 单层 |
| 全模型 N 段（N ≤ 10³，流式细切） | ≈ 2^-233 · N ≈ **2^-223** | 段数的 union bound 代价，可忽略 |
| PCS（HyperKZG/BN254） | ≈ **2^-100 ~ 2^-120** | q-type / DLOG 假设的具体强度 |
| **整体** | ≈ **2^-100** | **瓶颈在 PCS 假设，FS/sumcheck 项不构成瓶颈** |

### 4.2 Goldilocks/BaseFold 实例的警告

Goldilocks |F| ≈ 2^64：单层 ε ≈ 1601/2^64 ≈ 2^-53；单段 ≈ 2^-47；N=10³ 段 ≈ 2^-37。且分块使 grinding 面积缩小到单段（见 4.3），每次尝试代价 = 单段证明成本（秒~分钟级），2^37 次尝试对资源充足的攻击者并非天文数字。**结论：分块/流式生产场景必须固定 BN254/HyperKZG 实例**（论文仅称 BaseFold "plausible post-quantum secure"，是抗量子考量，非强度考量；如未来需要抗量子，须先在审计中重新做 ε 预算并评估 BaseFold 安全参数）。

### 4.3 Grinding 分析（分段特有）

- 非分段：攻破整链需一次成功的 grinding，每次尝试代价 = 全模型证明成本，成功概率 ≤ 2^-233（BN254）。
- 分段：只需攻破**一段**；每次尝试代价 = 单段证明成本（≈ 全链 1/N），成功概率 ≤ 段 ε（≈ 2^-236）。
- BN254 下 2^-236/次 × 单段成本 ⇒ 实际不可行；**这是分块的固有代价，论文 §4.5 同样接受**（其换取的是段间并行与内存有界）。
- 方案 A（§5）不消除段内 grinding（段 i 的种子固定后仍可通过变换段 i 自身承诺重试），它消除的是跨运行拼接/乱序/重放；段内 grinding 由 ε 直接压制。此点须如实记录。

### 4.4 其他安全属性边界

- **非零知识**：论文仅定义 ZK（Definition 10）；本方案见证/IO 以承诺与求值形式可见，不隐藏权重与输入。对"防篡改/防幻觉"（完整性）目标无影响；隐私需求另立项。
- **量化域**：证明对象是量化（i64）推理图；浮点等价性由量化校准保证，属非密码学问题（论文 §2.1/§3.1）。
- **FPGA/ASIC 证明方不受信**：安全性不依赖证明方诚实性，verifier 是唯一信任锚；验收方式 = 加速器产出的证明必须通过同一 verifier。

---

## 5 方案 A：全局串行 transcript（流式加固设计）

### 5.1 动机（当前派生的三个弱点）

| 编号 | 弱点 | 影响 |
| --- | --- | --- |
| W1 | 全局种子 = H(setup)，与本次执行实例无关，同一模型每次运行种子相同 | transcript 层面不绑定运行实例；跨运行的段证明在 transcript 层面可互换（IO 锚定在最终校验时拦截，但非 transcript 级防御） |
| W2 | 段间无随机性链：段 i 的种子不依赖段 0..i-1 的承诺 | 恶意分段证明方（FPGA 场景）可对任意一段独立 grinding；段顺序/来源在 transcript 层面不可检测 |
| W3 | 段总数与切分结构未在种子派生前绑定 | 段集合的完整性仅由 verifier 划分检查保证，transcript 不参与 |

### 5.2 派生规则

流式证明入口（`Prover::prove_streaming`，J 文档）维护一个**运行级全局 transcript** `S`：

```text
S₀ = T::new(b"model_stream_proving")
S₀ ← absorb(setup: ProverContext 承诺上下文)          // 与现有 G1 相同
S₀ ← absorb(commitment(io.input))                     // 输入张量的承诺先于一切段随机性（W1）
seed₀ = S.challenge_scalar()

对段 i = 0..N-1（按生成/证明顺序）：
  段 i 证明使用 initialize_transcript_for_chunk(seed_i)
  段 i 的 witness 承诺完成后（不必等整段证明完成）：
    S ← absorb(chunk_id_i, chunk_data_i, 承诺_i)      // (chunk_data, ChunkIOCommitments) 已可序列化
    seed_{i+1} = S.challenge_scalar()
```

- **W1**：输入承诺与 setup 一起先于一切段种子 ⇒ 种子绑定本次运行实例。
- **W2**：seed_{i+1} 依赖段 0..i 的承诺 ⇒ 段间随机性链；重放/乱序/跨运行拼接在 transcript 层面被破坏。
- **W3**：最终 `S` 吸收段数 N 与全部段结构摘要，verifier 顺序重放并断言段数一致。

### 5.3 与流水线/FPGA 的兼容性

- 段 i 的 seed_{i+1} 只依赖段 i 的 **witness 承诺**（prove_chunk 阶段 2 的产物），不依赖其 sumcheck/opening 完成。流水线中"段 i-1 承诺完成"早于"段 i 生成完成"，故 seed 的前传几乎不阻塞流水线（单依赖边，带宽 = 一个域元素 + 承诺字节）。
- 代价：段证明的随机性派生严格按段序串行；段内（sumcheck 内部 rayon）并行不受影响。若未来需要段间并行证明，可演进为 Merkle root 方案（收集全部段承诺 → root → seed_i = H(root ‖ chunk_id)），代价是两遍 transcript 重放——**本期不采用**。
- verifier 侧：顺序重放同样的吸收序列（verifier 已按 Vec 顺序逐段 verify_chunk，只需把"每段种子"从单挑战改为链式派生，并断言段数）。

### 5.4 与论文论证的关系

论文的知识可靠性论证只依赖"每个挑战在相关承诺之后派生"（BCS 前提）+ 段间承诺绑定 + 划分检查。方案 A 的串行链是该前提的**更严格实例化**（每个挑战额外绑定全部前序承诺），不引入任何更弱的假设；其正确性论证在审计文档中随实现一并补充（§7 实施要求）。

### 5.5 实现记录（v1.1，已落地）

| 要素 | 实现 |
| --- | --- |
| 全局 transcript | `T::new(b"model_stream_proving")`（与经典 `b"model_proving"` 域分离），持有于 `StreamingProver.global_transcript`（zkml/src/iop/streaming.rs） |
| setup 吸收 | 与经典一致：`Prover::initialise_transcript(ctx)`（absorb commitment_ctx） |
| 输入绑定（W1） | `verifier::bind_io_inputs_to_transcript`：`b"model_inputs"` ‖ 段数 ‖ 每个输入张量的（长度 ‖ 逐元素 append_scalars）。prover 侧在 `StreamingProver::new` 对原始输入 handle 做 `tensor() → pad_next_power_of_two → to_field`（与 `to_verifier_io` 对 `trace.input` 的变换逐操作一致）；verifier 侧对 `io.input` 调同一函数 |
| 段数绑定（W3） | `verifier::bind_chunk_count_to_transcript`：`b"model_chunks"` ‖ u64-le 段数 |
| 段种子 | `seed_i = S.challenge_scalar()`；段 i 的 chunk transcript 仍由 `initialize_transcript_for_chunk(seed_i)` 初始化 |
| proven chunk 吸收（W2） | `verifier::bind_proven_chunk_to_transcript`：对刚证明完的段做 `add_chunk_data_to_transcript(chunk)` ‖ `chunk_commitments.add_to_transcript(chunk_id)`，随后 `seed_{i+1} = S.challenge_scalar()` |
| **吸收顺序** | 段的**完成顺序**（= 流水线顺序），与最终 `Proof.chunk_proofs` 的 Vec 顺序严格一致；**不是** chunk_id 序。verifier 按该 Vec 顺序重放。段自身自包含（与经典路径相同），故 Vec 无需按 chunk_id 排序 |
| 验证入口 | `Verifier::verify_streaming` / 自由函数 `verify_streaming`（verifier.rs）；LLM 层 `LLMVerifierContext::verify_streaming`（与经典 `verify` 共享 EOS/移位业务校验，仅密码学重放路径不同）。**流式证明必须用流式验证入口**；两派生不兼容 |
| 验收 | `test_llm_driver_streaming_prove_gpt2`：流式证明经 `verify_streaming` 通过（含移位校验）；**负向断言**：同一证明经经典 `verify` 必须失败（固化派生区分；sumcheck verifier 以 panic 报不一致，测试用 catch_unwind 捕获） |
| 经典路径 | `verify`（单共享挑战）保持原语义；回归 `test_prover_steps_*`、`test_distribute_*` 全部通过 |

---

## 6 流式分段的影响评估（协议安全视角）

| 变化 | 协议影响 | 评估 |
| --- | --- | --- |
| 段由内存预算驱动、任意节点边界切分 | 边界张量进入相邻两段的 boundary 承诺集合，由既有机制绑定 | 无影响（DefaultChunkingStrategy 本就任意切；Add 约束只是减少 merge 开销的优化） |
| 段更小更多（N 增大） | union bound ×N、段证明固定开销（表证明每段一次）增加 | 安全性影响可忽略（§4.1）；性能影响由 J 文档处理 |
| 生成与证明流水线重叠 | 段证明顺序 = 生成顺序，挑战派生顺序不变 | 无影响；方案 A 的 seed 依赖与流水线方向一致 |
| 段数据证明后即释放（不落盘或落盘仅兜底） | verifier 不消费 trace 数据，仅消费 Proof 与 IO | 无影响 |
| 水平切分（二期） | replaced-nodes 边检查 TODO 需补齐 | 见 C6/§3.5，阶段 5 强制项 |

---

## 7 实施要求（随 J 文档的流式实现落地）

| 编号 | 要求 |
| --- | --- |
| R1 | 生产路径固定 BN254/HyperKZG 实例；禁止在分块/流式场景使用 Goldilocks/BaseFold（CI 校验或显式类型别名约束） |
| R2 | 阶段 1（现有挑战派生）与阶段 2（方案 A）各自通过 GPT-2 全链路 verify 测试；两版 verifier/prover 行为一致性有测试固化 |
| R3 | 本文档 §4.1 的 ε 预算表在实现段数上限变化时重算（预算函数：N 段 → union bound 项） |
| R4 | 阶段 5 实现水平切分时，补齐 model/context.rs:700 的 replaced-nodes 边检查，并在此文档修订记录中登记 |
| R5 | 方案 A 的 seed 链公式、吸收内容与顺序以本档 §5.2 为准实现；偏离须先修订本档 |

---

## 附录 A 代码索引

| 机制 | 位置 |
| --- | --- |
| 全局 transcript 与公共挑战 | prover.rs:579-586、prover_graph.rs:169-172 |
| chunk transcript 初始化 | chunking.rs:2213-2219、verifier.rs:684 |
| chunk 数据吸收 | chunking.rs:511-550（prover）、verifier.rs:155 |
| 见证承诺与吸收 | lookup/context.rs:420-491（batch_commit 420-422、承诺入 transcript 445-461、IO 承诺 490-491） |
| lookup 挑战（post-commitment） | prover.rs:616-620、verifier.rs:218-224 |
| 输出 claim（FS 点 + 真实 IO 对比） | prover.rs:629-667、verifier.rs:226-282、compute_claim iop/mod.rs:200-211 |
| common_point 派生 | prover.rs:697-712、verifier.rs:284-305 |
| sumcheck 逐轮 FS | dp-crypto sumcheck/prover.rs:196-215（append_and_sample） |
| PCS 批量 opening | prover.rs:884-890 |
| 输入锚定 | verifier.rs:427-492（verify_input_claim） |
| 划分/线束/边界检查 | model/context.rs:599-717（replaced-nodes TODO：700） |
| 跨 chunk 承诺一致性 | chunking.rs:622-712、verifier.rs:670-675 |
| Store 流量计数器（基线测量） | tenstore/src/metrics.rs、genstore.rs（fetch/disk_read/store 计数） |


---

## 补遗 v1.2：S1——transcript 连续采样同值修复（11-07）

**发现**：blake3 1.8.3 `finalize_xof(&self)` 每次从 XOF 流位置 0 读取——中间无吸收的连续
`challenge_scalar()` 返回**同一个值**。后果：① `read_challenges(n)` 的 n 个挑战全同
（`compute_claim` 的对角点、chunk 输出 claim 公共点）；② opening 的 RLC 系数逐 poly
连续采样（`prover.rs:119`）→ 全部相同，随机线性组合退化为 c·Σ——批处理绑定余量受损
（原文审计的"承诺先入 transcript、挑战后派生"顺序不变，此为派生**值**缺陷）。

**修复**（dp-crypto blake3.rs）：transcript 维持 pending XOF 流——吸收后首个采样从
位置 0 读（**与旧语义一致**），连续采样推进流（互异），再次吸收则流重置。prover 与
verifier 共用同一实现，重放一致性保持；prove_gpt2 端到端 + gemma-270m s=512 全链
revalidate 通过。

**影响**：① RLC 成为真随机线性组合（绑定余量恢复）；② 对角点变为真随机点；
③ **所有历史证明与新二进制互不兼容**（挑战值变化）——留痕结果仍对应各自当时的
二进制，无需重跑；④ 硬件 golden 向量以修复后语义为准（FPGA 文档 §2.3.4 v1.1）。
