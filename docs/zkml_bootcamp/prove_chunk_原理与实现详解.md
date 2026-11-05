# DeepProve `prove_chunk` 原理与实现详解

| 项目 | 内容 |
| --- | --- |
| 文档版本 | 1.0 |
| 日期 | 2026-11-03 |
| 适用代码 | 分支 `feat/streaming-trace-prove`（行号以该分支为准） |
| 读者对象 | 具备基本代数知识、对零知识证明了解有限的工程师 |
| 写作依据 | 全部结论直接来自源码逐行阅读，论文对照引自 `docs/zkml_bootcamp/deepprove.pdf`（下称"论文"） |
| 目标 | 读完本文后，能够独立讲清 `prove_chunk` 每一步在做什么、trace 数据如何被消费、证明如何逐层生成并合并，以及 sumcheck / MSM 两个底层引擎的工作机制 |

---

## 目录

1. [引言](#1-引言)
2. [基础概念](#2-基础概念)
3. [系统总览](#3-系统总览)
4. [prove_chunk 主流程](#4-prove_chunk-主流程)
5. [分步详解](#5-分步详解)
6. [trace 的消费路径](#6-trace-的消费路径)
7. [证明的生成与合并](#7-证明的生成与合并)
8. [与 DeepProve 论文的对应关系](#8-与-deepprove-论文的对应关系)
9. [复杂度与量级](#9-复杂度与量级)
10. [实现备注与易误解点](#10-实现备注与易误解点)
- [附录 A：符号表](#附录-a符号表)
- [附录 B：代码位置索引](#附录-b代码位置索引)
- [附录 C：参考文献](#附录-c参考文献)

---

## 1. 引言

### 1.1 本文解释什么

`zkml/src/iop/prover.rs:588` 的 `Prover::prove_chunk` 是整个 DeepProve 证明系统中**唯一的证明计算入口**：模型前向推理产生一份 trace（每个算子节点的输入/输出快照），`prove_chunk` 消费其中一个分片（chunk）的 trace，产出一个 `ChunkProof`。多个 chunk 的证明按协议规则合并成最终 `Proof`，交给验证方在毫秒级内完成校验。

本文自底向上组织：第 2 章先用可手工验算的小例子讲清 5 个基础构件（MLE、sumcheck、claim、承诺/MSM、transcript），第 3~4 章建立系统全景，第 5 章按执行顺序逐步拆解 `prove_chunk` 的 8 个步骤，其中每个重要子函数单独成小节，给出"职责 → 原理 → 代码走读 → 数值例子 → 产出"。

### 1.2 阅读前提

- 了解矩阵乘法、模运算（所有算术发生在素数域 `F = ark_bn254::Fr` 上，元素 32 字节）；
- 不需要任何零知识证明背景，第 2 章即是全部所需；
- 例子中的数值都用普通整数书写以便验算，实际运行时它们是模 `p ≈ 2^254` 的域元素（负数按 mod p 处理）。

### 1.3 术语约定

- `Element`：量化后的 i64 整数（推理世界的数据类型）；
- `F` / `Fr`：Bn254 标量域元素（证明世界的数据类型）；
- `∇` 或 "表"：MLE 求值表（见 §2.2）；
- `τ`：trusted setup 的毒物（toxic waste），`g1_powers = (G, τG, τ²G, …)` 是 SRS 基点序列；
- 文件引用格式 `path:line`，`zkml/` 为 crate 根；dp-crypto 指外部依赖 `Lagrange-Labs/dp-crypto`（源码位于 cargo git checkout）。

---

## 2. 基础概念

### 2.1 有限域与数据域化

**定义**。素数域 `F_p` 中加、减、乘按 mod p 进行，非零元素可求逆。推理产生的每个 i64 中间量都要映射进 `F`：正数直接映射，负数映射为 `p + x`（`quantization/mod.rs` 的 `to_field`）。

**为什么必须域化**。证明系统的一切运算（多项式求值、sumcheck、承诺）都定义在域上；i64 的溢出/符号语义在域中不存在。反过来，只要量化算术本身在 i64 中不溢出，"在域中等于"就等价于"在整数中等于"——这是整个证明正确性的算术基础。

**例子**。`-3 → p-3`；`7 → 7`。域中 `(p-3) + 10 ≡ 7 (mod p)`，与整数 `-3+10=7` 一致。

### 2.2 MLE：把张量变成多项式求值表

**定义**。把任意张量 pad 成 2^k 个元素后，这张表被重新解释为布尔超立方体 `{0,1}^k` 上的多线性扩展（Multilinear Extension，MLE）`f` 的**求值表**：表的第 `i` 项是 `f` 在点 `bit(i)` 处的取值，其中 `bit(i)` 是 `i` 的二进制展开，第 `j` 位对应第 `j` 个变量。`k` 称为变量数（num_vars）。

MLE 的关键性质：变量数固定时，一张 2^k 的表**唯一**确定一个多线性多项式；任意点 `r ∈ F^k` 上的取值可由表通过 EQ 加权求和算出（见 §2.4）。因此"对张量做证明"完全等价于"对这张表做证明"。

**代码**。`to_field() → pad_next_power_of_two() → into_mle()` 三步完成域化与表格化（tensor/quantization 模块）；`DensePolynomial` 的存储就是这张表，`evals_ref()` 取出它。

**例子**。张量 `[[1, 2], [3, 4]]`（形状 `[2,2]`）已为 2^2，变量 `(a, k)`（a=行、k=列）。本例约定表索引 `idx = a + 2k`，则表为：

```
idx:  (a,k) = (0,0)(1,0)(0,1)(1,1)
表:      [  3,    1,    2,    0 ]        ← 后文反复使用这张表
```

> **布局注**：仓库采用行主序展平，即**最后一个维度占最低二进制位**（对 `[2,2]` 行主序是 `idx = k + 2a`）。两种约定只改变表的排列顺序，不改变协议数学；本文例子统一用 `idx = a + 2k` 以便验算。

**规模直觉**。GPT-2 一层 `[512, 768]` 的激活张量 pad 后约 2^19 个元素，即一张 51 万项的表、19 个变量。

### 2.3 sumcheck：把"求和断言"折叠成一个点

**要证什么**。证明方 P 持有表 `g`（定义在 `{0,1}^n` 上），要向验证方 V 证明 `Σ_{x∈{0,1}^n} g(x) = Y`。V 没有时间逐项加（那要读完 2^n 张表），P 也不愿把表发给 V。

**协议**（论文 Appendix A.1 Protocol 1）。共 n 轮，第 i 轮：

1. P 发送单变量多项式 `g_i(X) = Σ_{其余变量} g(r₁,…,r_{i-1}, X, x_{i+1}, …, x_n)` 在 `X = 0, 1, …, d+1` 处的取值（d 为 `g` 对单个变量的最高次数，代码里统一发 `max_degree+1` 个点，见 dp-crypto `sumcheck/prover.rs:528-577`）；
2. V 检查相邻一致性：`g_{i-1}(r_{i-1}) = g_i(0) + g_i(1)`（第 1 轮用 `g_0(0)+g_0(1) = Y`，其中 Y 是 V 从别处已知的 claimed sum）；
3. V 发送随机数 `r_i`（本实现中从 transcript 抽取，见 §2.6）；
4. 双方把表沿变量 `x_i` 折叠：`g'(x_{i+1},…) = (1-r_i)·g(0,…) + r_i·g(1,…)`，表长减半（代码 `fix_var_parallel`，`sumcheck/prover.rs:634`）。

n 轮后表只剩 1 项：`g(r₁,…,rₙ)`。整个"2^n 项求和"断言塌缩成一条 **`f(r) = y`**——这就是 §2.5 即将定义的 claim。

**为什么作弊难（健全性直觉）**。若 P 第 i 轮发送了错误的轮多项式 `g_i'`（但 `g_i'(0)+g_i'(1)` 与真值一致以通过上轮检查），则 `g_i' - g_i` 是非零多项式，至多在 `d` 个点取零；随机 `r_i` 命中这些点的概率 ≤ `d/|F| ≈ d/2^254`。一旦未命中，P 的后续所有消息都在为一条**错误的多项式**做一致性，最终必然与真表在某处矛盾（最终 claim 与承诺打开不符）。逐轮错误概率求和，总健全性误差 ≤ Σ dᵢ/|F|。

**完整数值例子**。取 §2.2 的表 `f = [3, 1, 2, 0]`（变量 `(a,k)`），证明 `Σ f = 3+1+2+0 = 6`：

```
轮 1：g₁(X) = f(X,0) + f(X,1)
      f(X,0) = (1-X)·3 + X·1   = 3 − 2X
      f(X,1) = (1-X)·2 + X·0   = 2 − 2X
      g₁(X)  = 5 − 4X        → 发送 [g₁(0), g₁(1), g₁(2)] = [5, 1, −3]
V 检查：5 + 1 = 6 ✓   采样 r₁ = 5
折叠：f(5,k) = (1−5)·f(0,k) + 5·f(1,k) → 新表 [f(5,0), f(5,1)] = [−7, −8]

轮 2：g₂(X) = f(5,X) = −7 − X → 发送 [−7, −8, −9]
V 检查：g₂(0)+g₂(1) = −15 = g₁(5) = 5−20 ✓   采样 r₂ = 4
最终：f(5,4) = −7 − 4 = −11
```

验算：`f(5,4) = (1−5)(1−4)·3 + 5(1−4)·1 + (1−5)·4·2 + 5·4·0 = 36 − 15 − 32 + 0 = −11` ✓。

此后 V 只需相信"**那张已承诺的表在点 (5,4) 等于 −11**"这一条，用 §2.5 的承诺技术即可廉价核验。

**本仓库的引擎实现**（dp-crypto `sumcheck/prover.rs:116` `IOPProverState::prove`）有三个工程要点：

1. **虚拟多项式**：`VirtualPolynomialsBuilder`（`sumcheck/virtual_polys.rs:57`）把多张 MLE 注册为带编号的 witness（`Expression::WitIn(i)` 按指针去重），调用方给出的算术表达式（如 `W₀·W₁ + W₂`）被展开成单项式列表。一轮之内每个单项式独立逐点求值再按标量系数累加——于是"一次 sumcheck 同时处理多张表的乘加组合"。
2. **两阶段并行**（论文引用的 distributed sumcheck, arXiv:2210.00264）：先把超立方体按线程数切成子立方，Phase 1 各线程在**自己的子立方**上并行跑完 n 轮（每轮把各线程的轮多项式**加法归约**成一个再发 transcript，`sumcheck/prover.rs:23-103`）；Phase 2 合并各线程状态，对剩余的 `log₂(线程数)` 个"线程选择变量"补普通轮次（:196-208）。
3. **claimed sum 不显式入 transcript**：引擎只吸收各轮多项式与轮挑战；初始的 Y 由验证方从它已持有的 claim 重算（einsum、批量打开），或由证明方先行吸收（LogUp 的实例输出，`lookup/logup_gkr/prover.rs:163-166`）。两种约定并存，读验证代码时须分别对待。

**轮内代价**。每轮对每张表扫描剩余部分一次：总代价 `O(Σ表长 × 变量数 × 单项式内表数)`——这就是"sumcheck 成本 ∝ 节点张量元素数"的出处。

### 2.4 EQ 多项式与 claim

**EQ 多项式**。`eq(r, x) = ∏_j ( r_j·x_j + (1−r_j)(1−x_j) )`，在 `x = r` 处取 1、在任何其它布尔点取 0。它有整张 2^n 的求值表（`dp_crypto::poly::eq::evals`，逐维倍增生成）。

**关键恒等式（MLE 求值公式）**。对任意 `r ∈ F^n`：

```
f(r) = Σ_{x∈{0,1}^n}  f(x) · eq(r, x)
```

即：**表与 EQ 表逐点相乘再求和 = 表在随机点的取值**。这一条把"求值"化成"求和"，是 sumcheck 能承接一切求值主张的原因。

**数值例子**。`r = (5,4)`，表按 §2.2 约定排列：

```
eq((5,4), x) 在四个布尔点：
  (0,0): (1−5)(1−4) = 12      (1,0): 5·(1−4)  = −15
  (0,1): (1−5)·4    = −16     (1,1): 5·4      = 20
校验：Σ eq = 12−15−16+20 = 1 ✓（EQ 表元素和恒为 1）
Σ_x f(x)·eq = 3·12 + 1·(−15) + 2·(−16) + 0·20 = 36−15−32 = −11 = f(5,4) ✓（与 §2.3 一致）
```

**Claim**。`Claim{point, eval}`（`zkml/src/iop/claim.rs:26`）就是塌缩出来的 `f(r)=y`。它是本协议唯一的"流通货币"：每层证明吃进"输出端 claim"，吐出"输入端 claim"，最终所有 claim 都要落到已承诺的表上兑现（§5.8）。

**多 claim 合并（同一张表）**。若同一张表 `f` 在 k 个不同点上有 claim `(rᵢ, yᵢ)`，抽随机系数 `aᵢ` 后用一次 sumcheck 证：

```
Σ_{x∈{0,1}^n}  f(x) · ( Σᵢ aᵢ·eq(rᵢ, x) )  =  Σᵢ aᵢ·yᵢ
```

括号内是 k 张 EQ 表的随机线性组合——它只在"所有 rᵢ 的并集之外"近似为零，因此 sumcheck 的最终点 `r*` 同时承载了 k 条 claim 的信息；结束时得到合并 claim `f(r*) = Σᵢ aᵢ·(f(r*) 的份额)`。代码：`iop/same_poly.rs:83-106`（证明）、`poly_commit/prover.rs:140`（批量版，见 §5.8）。

### 2.5 多项式承诺、MSM 与 KZG

**要解决什么**。sumcheck 结束时的 claim `f(r)=y` 中，`f` 是证明方手里的表——V 凭什么相信这条 claim 说的是**那张约定好的表**（而不是 P 现编的另一张恰好满足等式的表）？承诺方案在推理开始前给每张表生成一个不可篡改的"指纹"，claim 最终用指纹核销。

**KZG 型承诺 = 一次 MSM**。把 MLE 表 `f₀,…,f_{N-1}`（N = 2^k）当作一元多项式 `u(X) = Σ fᵢ·X^i` 的系数，取 trusted setup 产生的基点序列 `Gᵢ = τ^i·G`（SRS，`g1_powers`），定义承诺：

```
C = u(τ) = Σᵢ fᵢ·(τ^i·G) = Σᵢ fᵢ·Gᵢ
```

这是一个 **MSM（Multi-Scalar Multiplication）**：N 个标量 × N 个椭圆曲线点逐个相乘再求和。代码实锤：`HyperKZG::commit` 调 `msm::poly_msm(g1_powers, poly)`（dp-crypto `arkyper/mod.rs:457-469`），`batch_poly_msm_cpu` 对每个多项式执行 arkworks 的 `VariableBaseMSM::msm`，**标量直接取 `poly.evals_ref()`**（`arkyper/msm.rs:41-57`）；开启 cuda/opencl feature 时切换为 GPU MSM（`msm.rs:60`）。

**绑定性的直觉**。不知道 `τ`（毒物已销毁）就无法伪造：若 P 想让同一 C 对应两套不同的表，需要找到非零的 `{δᵢ}` 使 `Σ δᵢ·τ^i = 0`，即构造出 `τ` 的整系数零化多项式——在离散对数假设下不可行。

**打开（opening）**。对 claim `f(r)=y`，P 提供一个简短的证明对象，V 用**常数次配对（pairing）**核验"C 确实是对在 r 处取值为 y 的多项式的承诺"。本仓库使用 HyperKZG（`Pcs = HyperKZG<Bn254>`，`zkml/src/testing.rs:9`），打开流程在 §5.8 结合源码详述。

**Pippenger（桶算法）**。朴素 MSM 需要 N×254 次点加。Pippenger 把每个标量切成 w 位一段，每段建 2^w 个桶累加，最后按位权合并——代价降到约 `N·254/w` 次点加，是所有 MSM 库（含 arkworks、GPU 内核）的标准做法。

**数值例子（玩具规模）**。表 `[3,1,2,0]`，`C = 3G₀ + G₁ + 2G₂ + 0G₃`。Pippenger 以 2 位窗口为例：标量 3 = `11₂`、1 = `01₂`、2 = `10₂` → 低段桶 `{1: G₁+2G₃…}`（示意），高段桶同理，最后 `Σ 2^{2t}·桶和(t)`。

### 2.6 transcript 与 Fiat–Shamir

交互式协议里 V 每轮发随机挑战；非交互化后，把"到目前为止所有应被双方看见的消息"按序喂进一个密码学哈希对象（transcript），挑战从哈希输出中截取（Blake3 实现，`dp_crypto/arkyper/transcript`）。只要 P 与 V **按同一顺序吸收同样的字节**，得到的挑战序列完全一致；而 P 无法"回滚" transcript 重选对自己有利的挑战——这正是 §2.3 健全性论证的前提。

因此，**读任何一段证明代码，先看它往 transcript 吸收了什么、按什么顺序**——吸收顺序就是协议本身。本文每一节都会标注吸收点。

---

## 3. 系统总览

### 3.1 证明目标

给定量化模型 M 和输入 x，证明方运行 M 得到输出 y，需向验证方证明：

> "存在一条按 M 的图结构执行的计算，从 x 出发，每步算子计算正确，最终产出 y。"

协议形态（论文 §4）：**逐算子（layer-by-layer）组合的交互式证明（PIOP）+ 多项式承诺方案（PCS）**。线性算子（矩阵乘/卷积）用 sumcheck 族协议（论文 Appendix B Protocol 3/4），非线性算子（GELU/softmax/RMSNorm/requant）用基于 LogUp 的查表论证（论文 §4，引 [PH23]），两者都以 GKR 式"逐层归约 claim"的方式串联；所有中间表用 HyperKZG 承诺，所有 claim 在收尾处一次性打开。

### 3.2 端到端数据流

```
                        ┌──────────────────────────────────────────────┐
模型 + 输入 x            │  前向推理（i64 量化算术）                       │
  ──────────────────▶   │  TraceRunner 在每个节点执行后记录 Step：        │
                        │  该节点的输入张量、输出张量、辅助量(proving_data) │
                        └───────────────┬──────────────────────────────┘
                                        │ (NodeId, Step) 逐步发出
                                        ▼
        ┌───────────────────────────────────────────────────────────┐
        │ StreamingProver（iop/streaming.rs）                        │
        │  · chunk 完成检测（on_node_complete, :229）                 │
        │  · 摘出该 chunk 的 steps 组装 chunk_trace（:259-264）       │
        │  · 专用 64MB 栈线程上调 prove_chunk（:272-285）             │
        │  · 证明吸收进全局 transcript → 派生下一 chunk 的 seed       │
        └───────────────┬───────────────────────────────────────────┘
                        ▼
              prove_chunk（本文主角，prover.rs:588）
                        ▼
              ChunkProof ──合并──▶ Proof ──▶ Verifier::verify_streaming
```

批式路径（`Prover::chunked_prove_local`，prover.rs:1043）与流式路径在 `prove_chunk` 处汇合，仅 trace 的来源与调度方式不同。

### 3.3 prove_chunk 的三个输入

| 参数 | 内容 | 来源 |
| --- | --- | --- |
| `chunk: ModelChunk` | 模型图的子图（`ChunkedGraph`）+ 跨块边界边表：`incoming_edges`/`outgoing_edges` 记录"哪条边、对端是哪个 chunk" | 切块策略在 run 前从图结构算出（流式无需 trace） |
| `chunk_trace: &Trace<Element>` | **只含本 chunk 节点**的 `HashMap<NodeId, Step>`（`model/trace.rs:29`）。`Step{node_inputs, node_outputs}`（:408）是端口级快照：`Vec<TensorHandle>`（i64 张量句柄）+ `proving_data`（LN 的 mean/std、softmax shift、argmax 等辅助量） | 流式：TraceRunner tap 攒到的 steps；批式：整份 trace 切片（`chunking.rs:553`） |
| `chunk_layers: &ModelLayersRef` | 节点 → 算子（Layer）的映射，从模型按子图过滤（`chunking.rs:579`） | 模型本体 |

`ModelChunk` 的节点有四种形态（prover.rs:723-746 的 match）：`OriginalNode`（未拆分的原算子）、`ChunkedLayer`（被拆分算子的一个分片，指向 `original_node_id`）、`SplitLayer`/`RecombinationLayer`（横向切分时插入的张量拆分/拼合节点，**不可证明**，只透传 claim）。

产出：`ChunkProof`（结构见 §5.9）。

### 3.4 上下文：setup 期预置了什么

`ProverContext`（`iop/context.rs`）持有三样静态资产，全 chunk 共享：

1. **PCS 参数**：SRS 的 `g1_powers`（基点序列，长度覆盖最大表）；
2. **模型承诺** `model_commitments`：每层权重张量的 MLE 承诺 + 查表输出列的承诺（setup 期一次 `batch_commit`，`poly_commit/context.rs:149-234`）；
3. **查表上下文** `lookup_ctx`：模型用到的所有表的定义。

transcript 初始化时即吸收全部模型承诺（`Prover::initialise_transcript`，prover.rs:579-586 → `ctx.write_to_transcript`，poly_commit/context.rs:333）——承诺先入哈希，后续一切挑战都"见过"这些权重指纹，权重无法被偷换。

---

## 4. prove_chunk 主流程

### 4.1 步骤总表

| 步骤 | 代码位置 | 做什么 | 主要开销 |
| --- | --- | --- | --- |
| 0 | prover.rs:600 | chunk 结构绑进 transcript | 忽略 |
| 1 | :608 → lookup/context.rs:233 | **witness 域化 + 批量承诺（MSM）** | MSM 主算之一 |
| 2 | :616-620 → iop/mod.rs:175 | 抽查表挑战 | 忽略 |
| 3 | :629-712 → iop/mod.rs:201 | **播种输出/边界 claim** | 轻（采样+求值） |
| 4 | :717-856 | **逆拓扑逐节点证明**（einsum sumcheck / LogUp 查表） | **主算（~84%）** |
| 5 | :862-870 → chunking.rs:717/746 | 边界 claim 登记 | 轻 |
| 6 | :875 → prover.rs:266 | 表侧 LogUp 批量证明 | 中 |
| 7 | :884 → poly_commit/prover.rs:97 | **全部 claim 归单点 + RLC + HyperKZG 打开（MSM）** | MSM 主算之一 |
| 8 | :891-914 | 组装 ChunkProof | 忽略 |

### 4.2 一句话版本

> 先把本 chunk 所有中间表域化、承诺、入 transcript（步骤 1）——表从此"定型"；再在输出端按 transcript 派生的随机点立下最初的求值断言（步骤 3）；然后从输出节点逆拓扑走向输入节点，每个算子用一次 sumcheck（或一次查表论证 + 一次链接 sumcheck）把"输出 claim"归约成"输入 claim"（步骤 4）；链条两端钉到 chunk 边界（步骤 5），表侧的查表义务用一次批量 LogUp 兑现（步骤 6）；最后把积攒的所有 claim 用一次 batched sumcheck 归到同一个点、随机线性组合成一条多项式、做**唯一一次** KZG 打开（步骤 7），打包返回（步骤 8）。

---

## 5. 分步详解

### 5.1 步骤 0：chunk 结构绑定 — `ModelChunk::add_chunk_data_to_transcript`

**代码**：`zkml/src/iop/chunking.rs:511-550`。

按顺序向**块级 transcript** 吸收：

1. `chunk_id`（u64 LE）；
2. 标签 `"incoming"` + 入边数 + 每条入边的 `(edge_id, 源chunk_id)`；
3. 标签 `"inputs"` + 本 chunk 内的模型输入边 id；
4. 标签 + 出边数 + 每条出边的 `(edge_id, 目标chunk_id)`；
5. 标签 `"outputs"` + 本 chunk 内的模型输出边 id。

**原理**。让本 chunk 的所有后续随机挑战依赖其切分结构。验证方在 `verify_chunk` 中调用**同一个函数**重放（`verifier.rs:196`），两侧逐字节一致；任何对图结构的篡改都会使 seed 之后的挑战序列错位，证明即失败。

**实现备注**：542 行在出边前写入的标签是 `"incoming"`（与 534 行入边标签相同，未写成对称的 `"outgoing"`）。因证明/验证共用同一函数，此标签不构成安全或正确性问题，仅为命名瑕疵。

### 5.2 步骤 1：witness 域化 + 批量承诺

入口 `instantiate_witness_ctx_for_chunk`（prover.rs:1171）→ `generate_lookup_witness_for_chunk`（lookup/context.rs:233）。这一步回答"证明的数据底座从哪来"。

#### 5.2.1 逐节点生成 witness MLE — `gen_lookup_witness`

**代码**：并行（rayon）遍历 `chunk.subgraph.forward_inners()`，每节点 `chunk_trace.get_step(node_id)` 取 Step，调算子的 `gen_lookup_witness`（分发见 lookup/context.rs:270-316）。

**以 GELU 为例**（`zkml/src/layers/activation.rs:313-344`）：取 `step_data.input_tensor_at(0)`（**trace 消费点**），查表论证需要把每个被查值表示为"表列切片"：

- 输入张量按表位宽**切片**成若干列 MLE（如 GELU 查表输入拆为高/低位两列）+ 一张**输出列 MLE**（`input_mle_evals` + `output_mle_evals`，全部 i64→Fr→MLE）；
- `element_counts`：`HashMap<值, 被查次数>`；
- 登记到 4 张表：`shift_check`、主表（GELU 表）、`zero_check`、`signed_zero_check`（activation.rs:334-339）——后三张用于排除负值/零的歧义（量化数在补码切片下有符号歧义，需要辅助表锁定）。

requant（`layers/requant/mod.rs:219`）、softmax、RMSNorm/LayerNorm、pooling、logits 各有变体，差异仅在切片数与辅助多项式（归一化因子、shift）。

#### 5.2.2 计算多重度（multiplicity）

**代码**：lookup/context.rs:317-362。对每张表：

1. 表列本身按值计数（表补 2 的幂时同一值会在多个位置重复出现）；
2. 对表**每个位置** j（值 `v_j`）生成 `mult[j] = 被查次数(v_j) ÷ 表中出现次数(v_j)`（除法在域中即乘逆元），得到一条与表等长的多重度多项式。

**为什么这样定义（LogUp 原理，见 §5.5.4）**：它使"表侧分数和 = 查询侧分数和"成立：

```
表侧   Σⱼ mult[j]/(β + v_j)
     = Σ_{值 v} (被查次数(v)/出现次数(v)) · 出现次数(v)/(β+v)
     = Σ_{值 v} 被查次数(v)/(β+v)
查询侧 Σᵢ 1/(β + qᵢ)                     ← 每次查询分子为 1
```

两侧逐值相等 ✓。`Σⱼ mult[j] = 查询总数`（好用的完整性自检）。

**数值例子**。表 `T = [10, 20, 10, 30]`（10 出现两次），trace 中查询序列 `q = [20, 10, 10]`：

```
mult = [ 1, 1, 1, 0 ]      （位置0: 2/2；位置1: 1/1；位置2: 2/2；位置3: 0/1）
Σ mult = 3 = 查询条数 ✓
恒等式：1/(β+10) + 1/(β+20) + 1/(β+10) + 0 = 1/(β+20) + 1/(β+10) + 1/(β+10) ✓
```

#### 5.2.3 边界张量做成 MLE — `to_be_committed_polys`

**代码**：`chunking.rs:437-478`。对每条入边取 `step.input_tensor_at(target_port)`、每条出边取 `step.output_tensor_at(source_port)`，pad → 域化 → MLE；按**源端口**去重（键为 `compute_commitment_id`，即源 `NodeOutput` 的哈希，chunking.rs:771）。

**意义**：这些是跨 chunk 一致性的实体。相邻两个 chunk 会对**同一张张量**各做一次独立承诺；验证方检查两个承诺相等（§7.3），即证明"下游吃到的 = 上游产出的"。

#### 5.2.4 一次性 batch_commit（MSM 主算）

**代码链**：所有多项式（各节点 witness 切片 + 多重度 + 入边 MLE + 出边 MLE）拼成一个 `Vec` → `ctx.commitment_ctx.batch_commit`（lookup/context.rs:417 附近）→ `CommittedPolynomial::batch_commit`（poly_commit/context.rs:58-88）→ `PCS::batch_commit`（arkyper/mod.rs:471）→ `batch_poly_msm`（msm.rs:41）。

切块规则（poly_commit/context.rs:103-121）：

```
num_chunks = ceil(2^num_vars / 2^20)，上限 128
每块 = 表的连续 2^20 项 → 每块一次 Pippenger MSM（基点 g1_powers）
```

上限 128 的原因是**验证方**要对每块做承诺合并，块数直接进验证开销（context.rs:108-110 注释）。

**产出与吸收顺序**（lookup/context.rs:443-482，顺序即协议）：

1. 逐节点的 witness 切片承诺 → 吸进 transcript，并登记进 `logup_witness[node_id]`（供 §5.5.4 使用）；
2. 多重度承诺 → 吸进 transcript（供 §5.6 使用）；
3. chunk 边界承诺 `ChunkIOCommitments{inputs, outputs}`（键 = 源端口承诺 ID）→ 吸进 transcript（供 §7.3 使用）。

**从这一刻起**，本 chunk 的所有表"定型"：后续任何 claim 都被解释为对这些已承诺表的求值主张。

### 5.3 步骤 2：ChallengeStorage

**代码**：`iop/mod.rs:175-199`。有查表层时，从块级 transcript 抽一个全局常数挑战（`b"table_constant"`）+ 每张表名一个挑战。它们在构造 LogUp 输入时把表的多个列线性组合成一个域元素（§2.6 的应用：列组合方式由随机挑战决定，防止证明方挑有利的组合）。

### 5.4 步骤 3：播种 claims

证明必须从输出端往回推，因此先在输出侧立断言。

#### 5.4.1 模型输出 claim

**代码**：prover.rs:629-667。遍历本 chunk 的模型输出边（`model_outputs_in_chunk`，chunking.rs:481——目标节点是模型 Output 节点的边），取源节点 Step 的输出张量，pad 域化后调 `compute_claim`（iop/mod.rs:201-212）：

```rust
let r_i = transcript.read_challenges(n_vars);   // 从块级 transcript 抽随机点
let y_i = tensor.into_mle().evaluate(&r_i)?;    // MLE 求值（EQ 加权和）
```

即"已承诺的输出表在随机点 r 等于 y"。eval 由证明方自算；验证方持有 `io.output` 真值，会在同一点重算核对（代码注释 :663-665 明确了这一点）。

**final-token 优化**（论文 §3.1）：LLM 场景只对**最后一行 logits** 建 claim，前 s−1 行不进证明——证明开销从 O(s²) 降为 O(s)。

#### 5.4.2 跨块输出 claim（common_point 机制）

**代码**：prover.rs:673-712。对本 chunk 每条**外向边**（输出被其它 chunk 消费）：

1. 取源端口输出张量 → MLE；
2. 维护**共同求值点** `common_point`：若张量变量数超过当前点长，用 `transcript.read_challenges(差额)` **续接增长**（:699-703）；
3. 每个张量在 `common_point` 的**前 num_vars 维**前缀上求值立 claim（:704-711）。

**效果**：本 chunk 每条出边都在**同一个 transcript 派生的随机点**（或其前缀）上被断言。由于点来自 transcript，验证方能重算出同一个点，从而把"上游块的出边 claim"与"下游块对同一张表的引用"对齐（§7.3）。

### 5.5 步骤 4：逆拓扑主循环（核心，约 84% 耗时）

**代码**：`for (node_id, node) in chunk.subgraph.backward_iter()`（prover.rs:717-856），从输出节点走向输入节点。**顺序不可换**：节点必须先持有"我的输出应等于什么"（下游给的），才能证"因为我的输入等于什么"（回传上游）。

每个节点执行：

| # | 动作 | 代码 |
| --- | --- | --- |
| 1 | 取 trace：`chunk_trace.get_step` | prover.rs:720 |
| 2 | 域化输出：`output_tensors() → to_field() → pad` | prover.rs:750-754 |
| 3 | 收集下游 claim：`claims_for_node` | prover.rs:762 → chunking.rs:388 |
| 4 | 合并同端口 claim：`flatten_and_merge_claims` | prover.rs:768 → :1121 |
| 5 | 证明：`op.prove`（或非可证明层透传） | prover.rs:812-833 |
| 6 | 回传输入 claim：`claims.extend` | prover.rs:840-845 |

（`record_layer_params`，prover.rs:159，仅是性能埋点，不参与协议。）

#### 5.5.1 子函数：`ModelChunk::claims_for_node`（chunking.rs:388-431）

**职责**：把"下游对我输出的断言"按输出端口归拢成 `BTreeMap<PortId, Vec<&Claim>>`。

**逻辑**：遍历本节点在子图内的 `outgoing_feeds`（每条"我的输出端口 p → 下游节点 t 的输入端口 q"连线）。对每条线：

- `(t, q)` 在 claim 寄存器 `claims_by_layers` 中（下游节点**在本 chunk 内**，已在更早的逆拓扑迭代中立过 claim）→ 取之；
- 否则下游在**别的 chunk**（本条线是出边）→ 从步骤 3.2 的 `chunk_output_claims` 按源端口取之（并对每个源端口只取一次，:399/:414 去重）。

一个端口挂多条 claim 是常态（扇出 + 各下游在不同点求值）→ 交给下一步合并。

#### 5.5.2 子函数：`flatten_and_merge_claims` 与 `same_poly`（prover.rs:1121-1167 / iop/same_poly.rs:83-106）

**原理**（§2.4 末）：同一张表在 k 个点的 k 条 claim，用随机系数 `aᵢ` 与 EQ 表合成**一次 sumcheck**：

```
Σ_x f(x)·(Σᵢ aᵢ·eq(rᵢ,x)) = Σᵢ aᵢ·yᵢ
```

**代码走读**（same_poly.rs:83-106）：`read_challenges(k)` 抽 k 个系数 → 每条 claim 生成 EQ 表 `evals(c_i.point())` → 表达式 `poly_expr * sum_expr`（sum_expr = `Σ Challengeᵢ·eqᵢ`）→ 一次 `IOPProverState::prove` → 合并 claim = `Claim{sumcheck 挑战点 r*, 首个最终求值}`。

**数值例子**。表 `f = [3,1,2,0]` 有两条 claim：`f(5,4) = −11`（§2.3 产物）与 `f(1,2) = y₂`。抽 `a₁=7, a₂=9`，sumcheck 证 `Σ_x f(x)·(7·eq((5,4),x) + 9·eq((1,2),x)) = 7·(−11) + 9·y₂`。结束后一条 claim `f(r*) = y*` 同时锚定两点。合并证明存入 `MergeClaimsProof`（prover.rs:87-98）随 ChunkProof 交验证方重放（`verify_merge_claims_proof`，verifier.rs:789-817）。

#### 5.5.3 子函数：einsum 的证明（矩阵乘 / 全连接）—— 最复杂的一条路

**入口**：`EinSum::prove_internal`（`zkml/src/layers/einsum/prove.rs:46`）。输入是若干条输出 claim（`ensure!(last_claims.len() == 输出数)`，:60-64）+ trace 中该节点的输入张量。

**要证的命题**（论文 Appendix B Protocol 3，General Matrix Multiplication PIOP）：

```
Σ_{k ∈ 收缩维}  LHS(行点, k) · RHS(k, 列点)  =  输出 claim 的值
```

以 `Y = X·W`（X: `[s,d]`，W: `[d,d']`）为例：行维 s、列维 d' 出现在输出中（被 claim 点固定），内积维 d 被求和消去（sumcheck 的自由变量）。

**第 ① 步：拆点**（prove.rs:138-142）。输出 claim 的点按输出形状 `split_point` 切成"行段 + 列段"。

**第 ② 步：轴分类** — `AxesMapping::sort_variables_to_axes`（axis.rs:503-580）。对 einsum 表达式的每个轴，标注 `FixedAxis`：

- `Outer(point)`：只出现在一侧输入的轴 → **固定**到 claim 点对应坐标；
- `Stacked(point)`：同时出现在 LHS 与 RHS、但**不求和**的轴（如多头注意力按 head 批量乘）→ 不固定，每个取值切一张独立小表，用 EQ 系数线性组合进表达式（批量 sumcheck，对应论文 Protocol 4 的 batched 语义）；
- `Contracted`：被求和的轴 → **保留**为 sumcheck 自由变量。

**第 ③ 步：固定与切表** — `mles_from_tensor`（axis.rs:1142-1234）。从**最后一个维度向前**扫描，维护 `(当前表, chunk_size, contraction_acc)`：

- 遇 `Outer` 维 → 用 EQ 点积把该维消掉。代码有两个分支（axis.rs:1158-1196）：
  - `chunk_size == 1`：数组按 `dim_size` 连续切块，每块与 EQ 表直接点积；
  - `chunk_size > 1`：按 `chunk_size·dim_size` 大块切，块内以 stride `chunk_size` 抽取做点积（保持先前保留维的布局）；
- 遇 `Stacked` / `Contracted` 维 → 只把 `chunk_size`（及 contraction 计数）乘上该维大小，不动数据。

扫完后得到长度 `stack_size × contraction_size` 的数据，按 `contraction_size` 切成 `stack_size` 张表，各自补零到 `2^ceil_log2(contraction_size)`（:1217-1233）。

**第 ④ 步：stacking 系数**（axis.rs:1043-1062）。stack 轴"固定在点 r"等价于 `Σⱼ eq(r,j)·P_j`；`stacking_coeffs[j] = eq(r,j)` 即线性组合系数（注释里给了 2×3 的展开示例）。

**第 ⑤ 步：建 sumcheck 表达式** — `build_einsum_expression`（op_info.rs:159-196）+ 批量挑战（prove.rs:158）：

```
Σ_{RHS输入 i}  γ · Σ_{j ∈ stack}  coeffs_ij · LHS_j(x) · RHS_{i,j}(x)
```

`γ` 是从 transcript 抽的 batching 挑战（把多个输出/RHS 并入一次 sumcheck；常规稠密层只有一个 RHS，表达式退化为 `γ·LHS(x)·RHS(x)`）。MLE 交 `VirtualPolynomialsBuilder` 注册（§2.3 要点 1）。

**第 ⑥ 步：跑 sumcheck**（prove.rs:184 → 引擎，§2.3）。变量数 `num_vars = ceil_log2(收缩维)`。

**第 ⑦ 步：收 claim**（prove.rs:188-253）。`get_mle_flatten_final_evaluations` 给出**每张参与表在最终挑战点的取值**；用 `reconstruct_full_point`（prove.rs:337-361）把挑战点按各输入张量的维度布局插回 `Contracted` 位置，拼成完整点。取值三分类：

- **活动输入**（激活值）→ `input_claims`，回传上游（步骤 6）；
- **常量权重**（W、bias）→ 记入 `commitment_map`（按 `CommitmentId`），不回传——它们指向 setup 期承诺，最终由 §5.8 的打开兑现；
- **bias** 不进 sumcheck，直接在由 claim 点导出的位置求值立 claim（prove.rs:193-214）。

若 einsum 输入数 > 2（LHS 被多个输出点共享），另跑 **input aggregation sumcheck**（prove.rs:260-301，表达式 op_info.rs:137）：`Σ_x input(x)·Σᵢ γᵢ·eq(pᵢ,x) = Σᵢ γᵢ·yᵢ`，把 LHS 的多条 claim 合一——与 same_poly 同构。

**完整数值例子（全流程可手工验算）**。

设 `X = [[1,2],[3,4]]`（s=2, d=2），`W = [[5,6],[7,8]]`（d=2, d'=2），则 `Y = X·W = [[19,22],[43,50]]`。变量约定：X 的变量 `(a,k)`（a=行、k=列），W 的变量 `(k,j)`，Y 的变量 `(a,j)`；表索引 `idx = 第一变量 + 2·第二变量`。

下游给的输出 claim：点 `(a=2, j=3)`，值 `Y(2,3) = 100`。验算：`eq(2,a) = [−1, 2]`，`eq(3,j) = [−2, 3]`，

```
Y(2,3) = (−1)(−2)·19 + (−1)(3)·22 + (2)(−2)·43 + (2)(3)·50
       = 38 − 66 − 172 + 300 = 100 ✓
```

轴分类：a → Outer（固定于 2）；j → Outer（固定于 3）；k → Contracted（自由）。`num_vars = ceil_log2(2) = 1`。

固定与切表（对照代码两个分支）：

```
W（chunk_size==1 直接点积分支，axis.rs:1158-1170）：
   R(k) = Σ_j eq(3,j)·W(k,j) = −2·W(k,0) + 3·W(k,1)
   R(0) = −2·5 + 3·6 = 8        R(1) = −2·7 + 3·8 = 10     → R = [8, 10]

X（chunk_size>1 stride 点积分支，axis.rs:1178-1195）：
   L(k) = Σ_a eq(2,a)·X(a,k) = −1·X(0,k) + 2·X(1,k)
   对展平表 [1,2,3,4] 以 stride 2 抽取：[1,3]·[−1,2] = 5；[2,4]·[−1,2] = 6   → L = [5, 6]
```

sumcheck（1 个变量，单项式 `L·R`，度 2，发 3 个点）：

```
claimed sum = γ·100（γ 为批量挑战，例中取 γ=1）
g(k) = L(k)·R(k) = (5+k)(8+2k)
发送 [g(0), g(1), g(2)] = [40, 60, 84]
V 检查：40 + 60 = 100 ✓    采样 r = 4
```

收 claim：

```
L(4) = 5+4 = 9   ← X 的输入 claim：X(2,4) = 9
   验算：X(2,4) = (1−2)(1−4)·1 + (1−2)·4·2 + 2(1−4)·3 + 2·4·4 = 3−8−18+32 = 9 ✓
R(4) = 8+2·4 = 16 ← W 的权重 claim：W(4,3) = 16
   验算：W(4,3) = (1−4)(1−3)·5 + (1−4)·3·6 + 4(1−3)·7 + 4·3·8 = 30−54−56+96 = 16 ✓
一致性：g(4) = 9·16 = 144 = L(4)·R(4) ✓
```

X(2,4)=9 作为输入 claim 回传上游；W(4,3)=16 进入 `commitment_map`，在步骤 7 打开权重承诺兑现。

#### 5.5.4 子函数：查表层的证明 — `prove_lookup_op`（generic_prove.rs:66-343）

GELU / softmax / RMSNorm / requant 等非线性算子没有低次多项式表达，走查表论证，分两截：

**第一截：LogUp GKR** — `new_batch_multiple_sizes_prove`（`lookup/logup_gkr/prover.rs:91-344`）。

原理（论文 §4 引 LogUp [PH23]）：把每次查表表示为分数——查询侧分子恒为 1，表侧分子是多重度 `mult`（§5.2.2 已保证两侧分数和相等）。"查表算对了" ⇔ 分数恒等式成立。直接验证分数和需要做域除法，LogUp 改为把分数运算编译成一个**分层电路**（每层做分子/分母的加法与交叉相乘），用 GKR 式逐层归约证明电路计算正确：第 `l` 层的值 = 第 `l+1` 层值的函数，逐层 sumcheck 归约，层数 = log₂(表大小)，**只由量化位宽决定、不随序列长度变化**。

本实现的三个机制（prover.rs）：

- **批量**：同变体的全部实例合成同一组轮次——每轮把各实例的层表达式相加成一个 sumcheck，用 `alpha`（合并分子/分母两列）与 `lambda`/`batching_challenge`（合并层内高低半区，:168-173 注释）做随机线性组合（:171-173 抽取，:272-273 传入）；
- **异构尺寸**：实例按变量数**降序**排列，最大者先开跑，小实例在对应轮次"滚入"（:183-203，`round_evaluations` 拼接），保证同轮结束；`claim_expr_multiplier = 2^(总层数−该实例层数)`（:221-222）对齐不同实例 claim 的量纲；
- **每轮一个 sumcheck**：EQ 表取当前 `sumcheck_point` 尾部坐标（:247-248），跑一次 `IOPProverState::prove` 后把新挑战续到点上（:291）。

结束时产出 `output_claims`（:307-330）：每个被查列/每张表在最终点上的取值 claim——其对象是步骤 1 已承诺的 witness 表。实例输出（circuit_outputs）在证明一开始就吸进 transcript（:163-166，这是 §2.3 要点 3 所述的"证明方先吸收 claimed sum"的场合）。

**第二截：linking sumcheck** — `LookupInputConfig::prove_linking_sumcheck`（`lookup/operation/inputs/proving.rs:26-114`）。

LogUp 只证了"witness 里的查表输入/输出对与表一致"，还需证明"**节点真正的输出张量**（我们手里有它的输出 claim，在 `last_claim.point()`）等于这些查表结果按算子语义的组合"。做法：以输出 claim 点与 logup 点构造 EQ 表（`variant.build_eq_polys`，:52-56），抽变体相关个数的挑战（:44-49），把"输出链接 + 求和链接 + 归一化链接"等多项检查合成**一个** sumcheck（:84-88）。变体差异：GLU 要带上未过表的那一半输入（generic_prove.rs:147-161）；softmax 要处理 shift（:162-170）；Normalisation 变体把"输出 = 输入 × 归一化 witness"作为乘积项并入（:171-205），归一化因子本身也是已承诺 witness。

**产出**（generic_prove.rs:260-335）：

- witness claim（分别在 logup 点、sumcheck 点、shift 点上）→ `prover.add_witness_claim_per_poly`（:325），进步骤 7 打开；
- 输入 claim（`variant.produce_input_claims`，:283-290）→ 回传上游；
- `GenericLookupProof{logup_proof, sumcheck_proof, evaluations, commitments}` → 存入该节点的 LayerProof。

### 5.6 步骤 5：边界 claims

**代码**：prover.rs:862-870；两个子函数在 chunking.rs。

- `compute_output_boundary_edges_claims`（chunking.rs:746-769）：把步骤 3.2 播种的出边 claim 按**源端口承诺 ID** 归拢；
- `compute_input_boundary_edges_claims`（chunking.rs:717-741）：从 claim 寄存器取"本 chunk 首层节点对**入边输入端口**立的 claim"，同样按源端口 ID（对端 chunk 的输出端口）归拢。

两类都经 `add_witness_claim` 注册（prover.rs:239-241）——它们是对步骤 1.3 边界承诺的主张，最终由步骤 7 打开兑现。

### 5.7 步骤 6：prove_tables — 表侧 LogUp

**代码**：prover.rs:266-320。查表层的"查询侧"已在步骤 4 证明，"表侧"在此证明：`create_logup_inputs` 用多重度 witness（步骤 1.2）与 ChallengeStorage 的挑战构造 `LogUpInput::Table` 型实例，`new_batch_multiple_sizes_prove` 一次批量跑完本 chunk 全部表（与 §5.5.4 第一截同一引擎，`ProofType::Table` 变体）。

产出的 claims 分流（prover.rs:288-313）：

- 多重度列的 claim → `add_witness_claim_per_poly`（对 witness 承诺的主张）；
- 表承诺了输出列时（`commit_output_column`），该列 claim → `add_table_claim`（对 **setup 期表承诺**的主张，prover.rs:233-237）。

验证方随后把查询侧与表侧两份 LogUp 证明的最终分子/分母 claim 配对核对（verifier.rs:587 的 izip），分数恒等式成立即查表义务闭合。

### 5.8 步骤 7：全部 claim 归单点 + 一次 KZG 打开

**入口**：`commit_prover.prove`（prover.rs:884-890 → poly_commit/prover.rs:97-138）。此时手里有几十上百条 claim：权重/偏置 claim（§5.5.3 第⑦步）、查表 witness claim（§5.5.4/§5.7）、边界 claim（§5.6）。每条都是"某张已承诺的表在某点等于某值"。

#### 5.8.1 子函数：claims batching sumcheck（poly_commit/prover.rs:140-255）

对每条 claim `(p, y)`：

1. 把 `y` 吸进 transcript（:170-172/:199-200）；
2. 生成 EQ 表 `evals(eq_point_per_chunk(p))`（:172/:201）。`eq_point_per_chunk`（poly_commit/mod.rs:44-50）把 claim 点**尾部的 chunk 选择位截掉**：大表承诺时按 2^20 切了块，点的高位是"选哪块"，在块内求值时退化为块的线性组合系数 `batching_coefficients_for_chunks`；
3. `build_sumcheck_expression`（poly_commit/mod.rs:83-140）把所有 (表, claim) 对拼成**一个**表达式，每项：

```
Challenge_j · ( Σ_m C_{j,m}·P_{j,m}(x) ) · EQ_{p_j}(x)
```

（注释 :89-97 完整解释了块分解：`P(r) = Σ_m C_m·P_m(r_低)`。）

4. 全部 witness 表与 EQ 表 zero-pad 到统一 `max_num_vars`（prover.rs:214-226），跑**一次** `IOPProverState::prove`（:240）；
5. `sumcheck_evals` = 所有被引用表在最终点 `r*` 的取值（:247）。

这就是论文 Appendix A.2 的 **Batched Sumcheck**：k 条不同点 claim → 1 个点上的 k 个取值。

**小例子**。两条 claim：`f(5,4) = −11`、`g(5,4) = y_g`（不同表、同点），挑战 `γ₁, γ₂`。sumcheck 证 `Σ_x eq((5,4),x)·(γ₁f(x) + γ₂g(x)) = γ₁·(−11) + γ₂·y_g`；结束后得到 `f(r*), g(r*)` 两个取值。

#### 5.8.2 RLC（随机线性组合）

**代码**：poly_commit/prover.rs:115-131。对每张被引用表从 transcript 抽系数 `cᵢ`，`DensePolynomial::linear_combination` 塌成**一条** `rlc = Σ cᵢ·Pᵢ`。验证方只需核验"rlc(r*) = Σ cᵢ·sumcheck_evalsᵢ"这一条等式——RLC 的随机性保证：只要任何一张表的取值是伪造的，组合后几乎必然对不上。**几十上百次打开需求被压缩成 1 次**。

#### 5.8.3 子函数：HyperKZG 打开（dp-crypto arkyper/mod.rs:329-383）

对"rlc 在点 r\* 处 = v"这一条，`PCS::prove` → `HyperKZG::open`，三个 Phase（全部自源码核实）：

**Phase 1：折叠**（:343-359）。对长度 2^ell 的表，`ell−1` 轮：

```
p_{i+1}[j] = point[i]·(p_i[2j+1] − p_i[2j]) + p_i[2j]
```

每轮表长减半，最后剩长度 2 的表（其值即目标求值，验证方可自算，无需发送）。**数值例子**：表 `[3,1,2,0]`、点 `(x₀,x₁)`：

```
p₁[0] = x₀·(1−3) + 3 = 3−2x₀        p₁[1] = x₀·(0−2) + 2 = 2−2x₀
p₂[0] = x₁·(p₁[1]−p₁[0]) + p₁[0] = 3 − 2x₀ − x₁
对照 MLE 求值：(1−x₀)(1−x₁)·3 + x₀(1−x₁)·1 + (1−x₀)x₁·2 + x₀x₁·0 = 3−2x₀−x₁ ✓
```

折叠就是把"逐变量固定"应用于一元视角的表。

**Phase 2**（:362-373）：对 `ell−1` 个折叠中间多项式做一次 `batch_poly_msm` 承诺（**这是步骤 7 的 MSM 主算**），吸进 transcript，抽挑战 `r`，构造三个特殊打开点：

```
u = [ r, −r, r² ]
```

**Phase 3**：`kzg_open_batch`（:186-234）——计算 `v[i][j] = f_j(u_i)`（`eval_as_univariate`：把 MLE 表当一元多项式在标量点求值），吸进 transcript，抽 `q_powers`，把 ell 个折叠多项式**再**线性组合成 `b_poly = Σ q^j·f_j`，对 `b_poly` 在 3 个点上做无余式 KZG 批量打开，得 witness `w = (w₀,w₁,w₂)`。

**验证侧**（:386-436）：

1. 便宜的一致性检查（:420-428）：`2r·Y[i+1] = r(1−xᵢ)(ypos[i]+yneg[i]) + xᵢ(ypos[i]−yneg[i])`——折叠递推在三点上的恒等式，先把各个求值互相锁定；
2. `kzg_verify_batch`（:237-320）：**一次 MSM + 一次配对等式** `e(L, H) = e(R, τH)` 同时验掉全部 3 个点的打开（t=3 特化实现把标量乘合并到 7 次，:258-274 注释）。

三点设计的意义：让一次配对核验多个求值，省去逐点打开的配对次数。

### 5.9 步骤 8：组装 ChunkProof（prover.rs:891-914）

```rust
ChunkProof {
    steps,               // 每节点的 LayerProof（einsum/lookup 各自的 sumcheck 证明 + 权重 claim）
    merge_claim_proofs,  // same_poly 合并证明
    table_proof,         // 表侧 LogUp 批量证明 + 多重度承诺（iop/mod.rs:139-144）
    commit,              // OpeningProof { batching sumcheck, sumcheck_evals, HyperKZG 打开 }
    chunk_data,          // chunk 结构 + 输入/输出边界承诺 + 每条出边的输出求值 output_evals
}                        // （ChunkProofData，iop/mod.rs:39-46）
```

回到流式协调器：`bind_proven_chunk_to_transcript`（verifier.rs:72-80）把 `chunk 结构 + IO 承诺` 吸进**全局** transcript，抽出的标量作为**下一个 chunk 的 seed**（streaming.rs:289-296）。

---

## 6. trace 的消费路径

| # | 消费点 | 代码 | 消费什么 |
| --- | --- | --- | --- |
| 1 | witness 生成 | `generate_lookup_witness_for_chunk` 遍历 `forward_inners`（lookup/context.rs:252-260） | 查表层节点的输入张量 → 位宽切片 MLE + 输出 MLE + 查值计数 |
| 2 | 边界承诺 | `to_be_committed_polys`（chunking.rs:437） | 入边目标端口的输入张量 / 出边源端口的输出张量 → MLE |
| 3 | claim 播种 | 模型输出边 + 外向边（prover.rs:629/673） | 输出张量 → `compute_claim` / common_point 求值 |
| 4 | 逐节点证明 | `backward_iter` 内 `get_step` → `op.prove`（prover.rs:720/818） | 每个节点的输出张量 = 被证的计算结果；`proving_data` 提供辅助量 |

**具体到一张 GELU 中间张量**：它在消费点 1 被切成位宽列并成为已承诺 witness（世界 A：承诺世界）；在消费点 4 它的**输出**被域化后作为 linking sumcheck 的右侧表（世界 B：张量世界），两世界由 §5.5.4 第二截的 sumcheck 焊接；它的**下游引用**（消费点 3/4 的 claim）最终在消费点 7 的打开中兑现。

**流式的意义**：消费点 1-4 全部发生在"该 chunk 的张量被 pin 在内存"的生命周期内，chunk 证完即释放（streaming.rs:207-228 注释）——任一时刻内存里只需一个 chunk 的张量，而非整份 trace。

---

## 7. 证明的生成与合并

### 7.1 层内：claim 链

每次 `op.prove` 产出一个 LayerProof 并吐出输入 claim；claim 寄存器 `HashMap<NodeInput, Claim>`（prover.rs:716）沿逆拓扑逐节点生长。**同一张表**的多条 claim 由 same_poly sumcheck 合并（§5.5.2）。

### 7.2 chunk 内：收口为一次打开

全部 claim 由 batched sumcheck 归到单点 `r*`（§5.8.1）→ RLC 归到一条多项式（§5.8.2）→ HyperKZG **一次**打开（§5.8.3）。几十上百次"打开承诺"的需求 = 1 条 sumcheck + 1 次三打开。

### 7.3 chunk 之间：三个验证时机制（无额外密码学计算）

`StreamingProver::finish`（streaming.rs:303-331）中 `proofs.into()` 仅把 `Vec<ChunkProof>` 装进 `Proof`（`iop/mod.rs:118-122` 的 From 实现）。跨 chunk 正确性由验证方三个机制保证（`Verifier::verify_streaming`，verifier.rs:755-787）：

1. **seed 链**。全局 transcript 吸收：setup（全部模型承诺）→ 模型输入（pad 后域化，`bind_io_inputs_to_transcript`，verifier.rs:47-58，证明方在 streaming.rs:168-176 以完全相同的变换准备输入）→ chunk 数（:62-65）→ 抽 seed₀。每验完一个 chunk，把其结构 + IO 承诺吸收（`bind_proven_chunk_to_transcript`，:72-80），抽下一个 seed。证明方（streaming.rs:165-179/289-296）与验证方逐字节重放同一序列 → chunk 无法被重排、替换、增删。
   （对照：批式验证 `Verifier::verify` 用**单一公共挑战**给所有 chunk 播种，verifier.rs:737——流式的顺序链是本分支引入的强化。）
2. **边界承诺相等**。`check_chunk_commitment_consistency`（chunking.rs:622-712；验证方在 verifier.rs:715 的 `prepare_verification` 中、逐 chunk 验证**之前**调用）：逐端口比对"下游 chunk 的入边输入承诺 == 源 chunk 的出边输出承诺"（键同为源端口承诺 ID）。同一张张量被两边独立 MSM 承诺，承诺相等 ⇔ 数据一致（找碰撞 = 破坏 MSF 绑定性）。此外 `chunk_data.output_evals` 提供出边在 common_point 上的取值，与下游 sumcheck 链交叉核对。
3. **首尾接模型 IO**。首 chunk 的入边对应模型输入（其 MLE 与 `io.input` 核对），末 chunk 的出边 claim 对上 `io.output`（验证方用真值在同点重算求值）。

---

## 8. 与 DeepProve 论文的对应关系

| 代码 | 论文位置 | 说明 |
| --- | --- | --- |
| 逆拓扑 claim 链、逐层 `op.prove` | §4 | *"Similar to the fabled GKR protocol, we compose these operator protocols in a layer-by-layer fashion"* —— 代码中的 claim 寄存器即该组合机制 |
| einsum sumcheck（§5.5.3） | Appendix B Protocol 3 / Protocol 4 | General MatMul PIOP：`Σ_k f_A(r1,k)·f_B(k,r2)`；Protocol 4 对应 stacking/batching 挑战 |
| 查表层 LogUp GKR（§5.5.4、§5.7） | §4 | *"lookup arguments based on LogUp [PH23] for proving non-linear layers"*；Protocol 5 为 Requantization 实例 |
| witness 承诺（§5.2） | §4 预处理 | LogUp GKR *"requires committing to the lookup table and the witness values"* |
| 权重/表 setup 承诺（§3.4） | §4 预处理 Model Commitment | `batch_commit` 一次完成 |
| 多 claim 归单点（§5.8.1） | Appendix A.2 Protocol 2 | Batched Sumcheck；A.1 Protocol 1 为单实例 |
| sumcheck 引擎（§2.3） | Appendix A.1 + 引用 [arXiv:2210.00264] | 多线程两阶段即 distributed sumcheck |
| 只证最后一行 logits（§5.4.1） | §3.1 | prover 开销 quadratic → linear |
| HyperKZG 打开（§5.8.3） | §3.2、§5 | PIOP 与 PCS 组合；§5 评测含 KZG / BaseFold 两种 PCS 变体 |
| 卷积的 FFT 矩阵委托（prover.rs:328 `delegate_matrix_evaluation`） | zkCNN 式矩阵求值分解 | 代码注释直接指向 zkCNN paper；DFT 矩阵按蝶形分解逐层 sumcheck |

---

## 9. 复杂度与量级

**MSM 侧**（承诺 + 打开）：

- witness 承诺块数 ≈ `Σ域化元素 / 2^20`（单表上限 128 块）——**随数据量（序列长度的平方项，来自注意力 score）增长**；
- 打开阶段 ≈ `ell−1` 个折叠承诺 + 3 点批量打开（§5.8.3 Phase 2/3）。

**sumcheck 侧**：

- 实例数由图拓扑决定（每可证明节点一次主 sumcheck；查表层每个节点 `d_L+1` 次归约，`d_L = log₂表大小` 只由量化位宽决定）；
- 每实例代价 `O(变量数 × 参与表总长 × 单项式表数)` —— 总量仍随序列长度增长，但不产生新的 MSM 调用。

**验证方**：每个 chunk 重放各 sumcheck（每轮只验低次多项式在少数点的取值）+ 一次三打开（1 MSM + 1 配对）+ 边界承诺比对——与证明成本差数个量级。

---

## 10. 实现备注与易误解点

1. **claimed sum 的两种约定**（§2.3 要点 3）：einsum / 批量打开的 claimed sum 由验证方从已持有 claim 重算，不进 transcript；LogUp 的实例输出由证明方**先**吸收（logup_gkr/prover.rs:163-166）。读验证代码时不要期待统一模式。
2. **EQ 点与 chunk 位**：claim 点的最后 `log₂(num_chunks)` 个坐标是块选择位，进 EQ 表前被 `eq_point_per_chunk` 截掉、退化为块组合系数（§5.8.1）——调试点长度时容易在这里困惑。
3. **MLE 索引布局**：行主序展平 ⇒ 最后一个维度占最低二进制位（§2.2 布局注）。手工复算表索引时必须先确认约定。
4. **`add_chunk_data_to_transcript` 的出边标签**写作 `"incoming"`（chunking.rs:542）：证明/验证共用同一函数故无影响，属命名瑕疵。
5. **非可证明层**（Split/Recombination）只透传 claim（prover.rs:829-833），不产生 sumcheck；但被横向拆分的节点会生成这两类节点并改写 trace（`replace_splitted_nodes`）。
6. **两条 transcript 并存**（流式）：全局 transcript 管 seed 链与 chunk 身份；块级 transcript（`b"model_chunk_proving"` + seed，chunking.rs:2334）管 chunk 内部的全部挑战。不要混用。
7. **证明线程栈**：prove 递归（sumcheck 表折叠 + 大结构递归析构）需要深栈，流式在 64MB 专用线程上运行（streaming.rs:56, 272-285）——在小栈测试线程里直接调 prove 会栈溢出，属预期行为。

---

## 附录 A：符号表

| 符号 | 含义 |
| --- | --- |
| `F`, `Fr` | Bn254 标量域 |
| `Element` | 量化 i64 |
| `∇`, "表" | MLE 求值表（2^k 项） |
| `n` / `num_vars` | MLE 变量数，表长 = 2^n |
| `eq(r, x)` | EQ 多项式及其求值表 |
| `Claim{point, eval}` | 求值主张 f(point)=eval |
| `τ`, `g1_powers` | setup 毒物与 SRS 基点 (G, τG, τ²G, …) |
| `C = Σ fᵢ·τ^i·G` | MLE 的 KZG 型承诺（一次 MSM） |
| `γ`, `α`, `λ`, `q` | 各协议层从 transcript 抽的随机挑战（batching / LogUp / 打开） |
| `r*` | sumcheck 最终挑战点 |
| `d_L` | 查表层 LogUp 折叠轮数 = log₂(表大小) |
| `mult[j]` | 表位置 j 的多重度 = 被查次数 ÷ 表内出现次数 |

## 附录 B：代码位置索引

| 主题 | 位置 |
| --- | --- |
| prove_chunk 主体 | `zkml/src/iop/prover.rs:588` |
| chunk 结构入 transcript | `zkml/src/iop/chunking.rs:511` |
| witness 生成 / 多重度 / 批量承诺 | `zkml/src/lookup/context.rs:233` / :317 起 / :417 附近 |
| GELU witness 细节 | `zkml/src/layers/activation.rs:313` |
| batch_commit 切块 | `zkml/src/poly_commit/context.rs:58, 103-121` |
| MSM 实现 | dp-crypto `arkyper/msm.rs:41-93`；commit `arkyper/mod.rs:457` |
| compute_claim / ChallengeStorage | `zkml/src/iop/mod.rs:201` / :175 |
| claims_for_node | `zkml/src/iop/chunking.rs:388` |
| same_poly | `zkml/src/iop/same_poly.rs:83` |
| einsum 证明 | `zkml/src/layers/einsum/prove.rs:46`；fix_axes `axis.rs:478/503/1142`；表达式 `op_info.rs:159` |
| 查表层证明 | `zkml/src/lookup/operation/generic_prove.rs:66`；linking `inputs/proving.rs:26` |
| LogUp GKR 批量引擎 | `zkml/src/lookup/logup_gkr/prover.rs:91` |
| 边界 claims | `zkml/src/iop/chunking.rs:717/746` |
| prove_tables | `zkml/src/iop/prover.rs:266` |
| 批量打开（batching sumcheck / RLC） | `zkml/src/poly_commit/prover.rs:97/140`；表达式 `poly_commit/mod.rs:83` |
| HyperKZG open/verify | dp-crypto `arkyper/mod.rs:329 / :386`（kzg_open_batch :186，kzg_verify_batch :237） |
| ChunkProof / Proof | `zkml/src/iop/mod.rs:68 / :103` |
| 流式协调 | `zkml/src/iop/streaming.rs:229/259/272/289/303` |
| 验证方重放 | `zkml/src/iop/verifier.rs:196/715/721/755`；bind 系列 :47/:62/:72 |
| sumcheck 引擎 | dp-crypto `sumcheck/prover.rs:116`（Phase1 :23-103，轮函数 :490，fix_var :634）；虚拟多项式 `sumcheck/virtual_polys.rs:57` |

## 附录 C：参考文献

1. DeepProve: Verifiable Private Large Language Model Inference. Lagrange Labs / UIUC / HKUST / Yale. `docs/zkml_bootcamp/deepprove.pdf`（§2 预备、§3 技术总览、§4 核心设计、§5 评测、Appendix A/B 协议细节）.
2. Thaler. Time-Optimal Interactive Proofs for Circuit Evaluation (sumcheck / GKR), CRYPTO 2013.
3. Paden et al. LogUp: Efficient logarithmic (lookup) arguments, ePrint 2023（论文引作 [PH23]）.
4. Zhang et al. HyperKZG: KZG-style multilinear polynomial commitment from two groups.
5. CP 等. Distributed sumcheck（多线程两阶段实现依据）, arXiv:2210.00264.
6. zkCNN（卷积 FFT 矩阵求值委托的出处；代码注释 `prover.rs:322` 直接引用）.
