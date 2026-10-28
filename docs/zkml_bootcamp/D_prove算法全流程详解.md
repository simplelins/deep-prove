# D · Prove 全流程详解（面向 FPGA 实现）——从代码入口到最底层算子

> **本文给谁看**：给要**读 `zkml` 源码、并把一次 zk prove 落到 FPGA 硬件**的开发者。目标不是学术定义，而是把你按代码路径带到底：**每一步到底在做什么计算、喂什么数据、吐出什么、代码在哪一行、落到 FPGA 上怎么实现**。
>
> **一句话总览**：一次 prove 唯一的计算入口是 `Prover::prove_chunk`（`iop/prover.rs:542`）。它内部分 **A→B→C→D 四个计算段**，之外是主机在两侧干的"壳"（入口/切块/调度 + 序列化），不算证明计算。**你读代码、做 FPGA，都从 `prove_chunk` 这一个函数出发。** 四段：
>
> ```
>   A  witness 域化 + 批量承诺    i64→Fr→MLE→batch_commit(MSM)      "数据底座 + 一次主算(~4%)"
>   B  播种 claims                输出/边界 求值点+采样→Claim          轻，为 C 播种(~0.3%)
>   C  节点级 sumcheck            逆拓扑逐节点 op.prove + 查表          ★主计算(~84%)
>   D  合并 + 打开                合并sumcheck→RLC→HyperKZG open      主算收口(~10%)
> ```
>
> **为什么是"A、B、C、D 四段"而不再用别的分法**：硬件只碰 `prove_chunk` 体内的计算、不碰控制流。`A→B→C→D` 正好是这段代码里四个**能独立看、独立实现**的计算单元。壳（入口/序列化）在函数体外由主机做；i64→Fr 域化是随用随转的动作、不是独立段，散落在 A、C 里。**本文从此不再用"8 个段"之类的旧分法，只用这 4 段 + 壳。**

> 全文字节数较多，先看**章节目录**（见下）。四个计算段各占一章（第 3~6 章），每章按「这在干嘛 → 读代码看哪 → 数据长什么样 → FPGA 怎么做」展开；壳（第 2、7 章）与贯穿底座（第 8 章）为辅。再往下的微算子/密码单元/算子族下钻见 [附录 G](#附录-g--微算子--密码单元--算子族下钻)；调度/带宽/瓶颈见 [B](B_工作负载与瓶颈判定.md)；trace 存储量见 [F](F_trace存储量核算.md)；分割合理性/耗时占比见 [附录 F](#附录-f--四段分割合理性--实测核验)。

> **符号**：`F=Fr=ark_bn254::Fr`(32B)；`Element=i64`(8B)（域化前）；`∇`(MLE)=`DensePolynomial<Fr>` 求值表，`2^n` 个元素；`Trace=HashMap<NodeId,Step>`；`G1=BN254 曲线点`。**源码位置一律 `文件:行号`。**

---

## 目录

1. [总貌与四段逻辑](#1-总貌与四段逻辑)　1.1 调用树 / 1.2 竖直管线 / 1.3 落点 / 1.4 贯穿线
2. [壳 · 入口 / 切块 / 调度](#2-壳--入口--切块--调度)（主机侧，不算计算段）
3. [段A · witness 域化 + 批量承诺](#3-段a--witness-域化--批量承诺)　3.1 域化 / 3.2 批量承诺（MSM）
4. [段B · 播种 claims](#4-段b--播种-claims)（轻，为 C 播种）
5. [段C · 节点级 sumcheck](#5-段c--节点级-sumcheck)（主计算）
6. [段D · 合并 + 打开](#6-段d--合并--打开)　6.1 合并 sumcheck + RLC / 6.2 KZG 打开
7. [壳 · 序列化 / 证明打包](#7-壳--序列化--证明打包)（主机侧，不算计算段）
8. [贯穿底座与关键数据结构](#8-贯穿底座与关键数据结构)

- **附录**：A 调用次数/规模 ｜ B 算子清单 ｜ C FPGA 阶段化 ｜ D 算子→源码 ｜ E 阅读顺序 ｜ F 四段分割合理性+实测核验 ｜ **G 微算子·密码单元·算子族下钻**

---

## 1. 总貌与四段逻辑

**总貌 = 一张真实调用树**：从 demo 按一次键 → 到 `prove_chunk`（真正的证明计算入口）。先钉死"prove_chunk 被谁、在哪一行调出来"，再展开它内部 A→B→C→D 四段。留给读者一句话：**所有证明计算都在 `prove_chunk` 里；它之前的一切只是"准备数据 + 建调度图"的壳，它之后的序列化也是"打包"的壳。**

**1.1 demo 到 prove_chunk 的真实调用树**（代码逻辑，按行号可点）：

```
zkml/src/bin/bench/llm.rs:317   let (proof, io) = driver.prove(&prover_ctx, trace)?;
        │
        ▼
model/llm.rs:774  Driver<N>::prove                                  ←【应用层】模型专用壳
        ├─ :788 DefaultChunkingStrategy::from(&trace)               选切块策略（默认整模型 1 块）
        └─ :789 self.chunked_prove_local(ctx, trace, Some(1), strategy, ())
             │
             ▼
iop/prover.rs:995  Prover::chunked_prove_local                      ←【证明层】真正入口
        ├─ :1019 ctx.split_in_chunks(num_chunks, strategy)         切模型图→子图+边界边+替换节点
        ├─ :1021 full_trace.replace_splitted_nodes(...)            横向切时标记被切节点
        ├─ :1023 full_trace.to_verifier_io()                       域化公共 IO（verifier 核对对象）
        ├─ :1029 build_local_execution_graph::<...>(chunks)        把 chunks 组装成「执行图」ProverGraph
        │         └─ build_execution_graph_internal(prover.rs:906)
        │               node0=ProverSplit(0)│ C×ChunkProver(i)│ Final ; :918-936 加边连成链
        ├─ :1030 graph_inputs(full_trace, &graph)                  Σ_执行图源节点喂 full_trace 作为根输入
        └─ :1032 run_execution_graph::<Ex>(graph, inputs, &ctx, ())    ← 交给调度器跑
             │
             ▼
iop/prover.rs:962  run_execution_graph                              ←【调度层】只发信号、不着计算
        └─ :974 Ex::run(&config, scheduler, inputs, context)      Ex=SequentialExecutor(scheduler.rs:161)
             │
             ▼
graph/scheduler.rs:161  SequentialExecutor::run                    ← 逐个取 ReadyNode 喂给 node.run
        └─ scheduler.rs:257  ReadyNode::run  →  self.node.run(ctx, inputs)
             │                                  node = Colored<ProverGraphNode, C>
             ▼
iop/prover_graph.rs:157  ProverGraphNode::run                       ←【真正 dispatch】match 三种图节点
        ├─ ProverSplit   (:159-187)  transcript.challenge_scalar(L171) → chunk.chunk_trace(L176)
        ├─ ChunkProver   (:190-207)  initialize_transcript_for_chunk(L200) → Prover::new(L201)
        │                          → chunk.chunk_layers(model)(L202)
        │                          → 【prover.prove_chunk(...)】(prover_graph.rs:203) ★
        └─ Final         (:209-221)  收集 ChunkProof → FinalProof(L221)
             │
             ▼  (★ 这一行进 prove_chunk，A→B→C→D 四段全部从这里进入)
iop/prover.rs:542  Prover::prove_chunk                             ← 真正的证明计算体
        ├─ :562  instantiate_witness_ctx_for_chunk   (A·域化+承诺)
        ├─ :583-666  output_claims_by_port + chunk_output_claims   (B·播种 claims)
        ├─ :671-808  for (...) chunk.subgraph.backward_iter() → op.prove  (C·节点级 sumcheck)
        ├─ :827  prove_tables                          (C 查表层 LogUp)
        ├─ :836  commit_prover.prove → ... → RLC → HyperKZG::open  (D·合并+打开)
        └─ :860-871  拼 ChunkProof 返回                    (壳·序列化上游)
```

> **★ 从 demo 怎么快速对到 prove_chunk**：按 `llm.rs:317 → driver.prove` 走，中途会看到 `Driver<N>::prove`（model/llm.rs:774）→ `chunked_prove_local`（prover.rs:995）→ `run_execution_graph`（prover.rs:962）→ `SequentialExecutor::run`（scheduler.rs:161）→ `node.run`。**到 `node.run` 这里看起来"断了"——因为真正的 `prove_chunk` 不在 scheduler 层，它藏在下一步 `ProverGraphNode::run`（prover_graph.rs:157）的 `match` 里**：`ChunkProver` 分支那行 `prover.prove_chunk(...)` 在 **prover_graph.rs:203**。再走一步就到了。**文档 D 的主链就是从这行进去的 A→B→C→D。**

> **正名（避免"包一层"的混乱）**：仓库有**两条**到 prove_chunk 的壳，别混——
>
> - **`Driver<N>::prove`（model/llm.rs:774）**：LLM 应用专用壳，demo 走的就是它；内部直接调 chunked_prove_local。
> - **`Prover::prove`（iop/prover.rs:1040）**：zkml 通用壳，又包一层 chunked_prove_local→SequentialExecutor。
>   两条最后都在 `chunked_prove_local`（prover.rs:995）汇合 → 进 `ProverGraphNode::run` → `prove_chunk`。**本文 A→B→C→D 讲的全是 `prove_chunk` 内部（prover.rs:542 起），与用哪条壳无关。**

**1.2 四段竖直管线：一次 prove 从入口到序列化的主干**

每一行读作「喂进什么 → 哪个算子 → 得到什么」。**域标**：`i64`=8B 域化前；`F`=Bn254Fr；`G1`=曲线点；`∇`=MLE 求值表。**主管线 A→B→C→D 全部发生在 `prove_chunk`（prover.rs:542）函数体内**；壳在它外面，见下注。

```
[壳·主机侧，不算计算段]  入口/切块/调度
   full_trace(i64) + 权重 ──split_in_chunks→chunk_trace + 执行图(0=Split,C×ChunkProver,C+1=Final)

A  witness 域化 + 批量承诺
   Element=i64 张量 ── 逐节点 to_field(i64→F)→pad 2^n→into_mle──▶ ∇ MLE
   逐节点 ∇ ──切块(2^20)+batch_poly_msm(Pippenger)──▶ 每块 1 个 G1 承诺
B  播种 claims
   节点输出 ∇ ──evals(point)+EQ+挑战采样──▶ Claim{ point, value }
C  节点级 sumcheck
   虚拟多项式(多∇×Challenge) ──逐轮 fold+build_uni_poly──▶ 轮多项式 + 输入 claims
   （查表层走 LogUp/GKR）
D  合并 + 打开
   全 claims ──EQ+batching→单点；linear_combination(RLC)──▶ 1 条 rlc_poly
   rlc_poly+单点 ──折叠 2^m→常数→承诺折叠 poly→3点 u=[r,−r,r²]──▶ OpeningProof

[壳·主机侧，不算计算段]  序列化
   OpeningProof+各ChunkProof ──postcard+CanonicalSerialize──▶ Proof 字节流
```

> **四段逻辑的切分判据 = "哪些是真计算、且能独立看/独立实现"**：A 立数据底座并首次承诺；B 播种 claim（轻，取样+求值）；C 主循环逐节点 sumcheck（占 ~84%，主计算）；D 把全 claim 合拢成单点并打开（占 ~10%）。**入口/切块/调度 与 序列化 在 `prove_chunk` 之外，是主机侧的壳，不做证明计算；i64→Fr 域化不是独立段，而是 A、C 里"逐节点随用随转"的动作（`section.output_tensors() → to_field().pad_next_power_of_two()`，见 A:603-606 / C:704-708）。**

**1.3 四段在 `prove_chunk` 里的真实落点 + 3 根贯穿线**

代码里 A/B/C/D **不是 4 次顺序调用**，而是被塞进 `prove_chunk`（prover.rs:542）里的**一次"承诺先行 + 一段逆拓扑循环 + 一个收尾合并"**。四段是**数据在各层的计算流动**，不是 4 份整齐代码块。先给**落点表**（prover.rs:542-871 真实调用顺序），再讲拧成一股的 **3 根贯穿线**。

| 代码位置                                                                                                       | 做的事                                                                                                            | 段                            |
| -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| **:562** `instantiate_witness_ctx_for_chunk`                                                           | 逐节点生成 witness MLE(**域化 i64→Fr→MLE**) + **一次性** `batch_commit`→`chunk_io_commitments` | **A：域化+承诺**        |
| **:570-573** `ChallengeStorage::initialise`                                                            | 响应对应的 lookup 挑战                                                                                            | B 的一部分（挑战存储）        |
| **:583-621** `output_claims_by_port`                                                                   | 对模型输出 tensor `to_field().pad_next_power_of_two()` → `compute_claim` 采样                                | **B：播种输出侧 claim** |
| **:627-666** `chunk_output_claims`                                                                     | 对外向边 tensor 同上 → transcript 采**共同求值点 `common_point`** → `mle.evaluate` 出 eval → Claim   | **B：播种边界 claim**   |
| **:671-808** `for (node_id, node) in chunk.subgraph.backward_iter()` → `op.prove`                   | **逆拓扑**逐节点跑 sumcheck（域化该节点输出后立证），量出/回传 claims                                       | **C：节点级 sumcheck**  |
| **:815-822** `compute_*_boundary_edges_claims`                                                         | 把边界边的 poly 挂进 witness claim                                                                                | C 的辅助                      |
| **:827** `prove_tables`                                                                                | LogUp 查表证明                                                                                                    | C 的查表层                    |
| **:836** `commit_prover.prove` → 内部 `claims_batching_sumcheck`(:140) → RLC → `HyperKZG::open` | **一个**合并 sumcheck + 线性组合 + 打开                                                                     | **D：合并+打开**        |
| **返回 `ChunkProof`** (**:860-871**)                                                             | Final 汇集+序列化                                                                                                 | 壳（序列化上游）              |

> **读上表的关键**：A 在最前（:562）；B 在中间（:583-666，分"输出 seed"+"边界 claim"；挑战存储 :570 也归 B）；C 是一个逆拓扑 for 循环（:671）；D 在末尾一个 `commit_prover.prove`（:836）。**域化归段 A 主导、段 C 即时复用**（随取随弃），不单独成段。

**把四段拧成一股的是下面 3 根贯穿线**：

1. **Claim 是唯一跨段传递的"货币"（B 定义"证什么"，C 回溯"证出什么"）**。B 播种条目/输出侧 `Claim{求值点,求值}`（:583-621 播种模型输出、:627-666 播种外向边、`compute_claim`(:620)/`mle.evaluate`(:659)）；C 的逆拓扑循环里，每个节点**先**用 `chunk.claims_for_node(node_id, &claims, &chunk_output_claims)`(:717) 吃下游已确立的 claim，**再** `op.prove` 求出本节点输入又生成新 claim，最后 `claims.extend(...)`(:792-797) 回传上游。**→ 一个节点的"输出 claim"成为更上游节点的"输入 claim"，claim 沿逆拓扑一条链传**。**B 只是 C 的"播种"，两者同一次逆拓扑循环的前后两拍，切开会引入 claim 往返。**
2. **单个共享 transcript（Fiat-Shamir）把所有随机挑战串成时序**。从入口壳的 `ProverSplit`(:171 `challenge_scalar`) → B 的 `compute_claim`/`common_point` 采样 → C 每轮 sumcheck 的挑战 → D 的 RLC 与 KZG 挑战，**全部从同一个 `self.transcript` 依次读**。代码里靠 `&mut self.transcript` 一路传、没有显式"关联变量"。**挑战是全程唯一的"逐轮高频"件——必须贴近计算引擎内建，否则 A/B/C/D 之间退化成逐轮小包往返。**
3. **MLE(∇) 是 A、B、C、D 共同的"数据底座"**。A 把 i64 张量 `into_mle` 成 ∇ 求值表并 `batch_commit` 承诺；B 在 ∇ 上采点算 claim；C 在 ∇ 上逐轮 sumcheck；D 把所有 ∇ 按挑战 `linear_combination` 成 1 条 rlc_poly 再打开。**物理上同一批 ∇ 历经 4 道变换**：`into_mle → evaluate → fold → linear_combination/kzg_open_batch`。此外还有一份**静态权重/查表 ∇（setup 预置，不随 trace 走）**参与 C、D 计算。

> **一句话合拢三根线**：**B 先定"证什么"（从 transcript 采样的输出/边界求值断言=claim）；C 逆拓扑回溯，用 `op.prove` 把"输出 claim"逐节点归约成"输入 claim"回传上游，形成 claim 链；A 早已把全图 witness MLE 承诺好，C 的每条 claim 最终都落到一条已承诺的 MLE 上；D 把残留在 claim 链末端的多项式按挑战批量线性组合、一次合并 sumcheck 归约到单点、再 KZG 打开；入口/序列化只是外包的"切块调度"与"打包"。**

**两条根本规律**（贯穿全篇，先记住）：

- **MSM 次数 ∝ 域化元素总量 ΣQ**（随 trace 的 `s+s²` 涨），不由节点数决定——批量承诺是"一次调用吃下全部 witness，内部按 2^20 切块"，每块一个 Pippenger MSM。
- **sumcheck 实例数 ∝ 图节点拓扑（可证明节点数 V），不随 s 涨**；但**每个实例的规模 ∝ 该节点张量 2^n**（score 的 s² 尤其），所以 sumcheck"总代价"仍随 s 涨。

**执行位置速查**（FPGA 对照，细到算子）：

| 步 | 算子                                                                | 需要的数据                               | 归属                                               |
| -- | ------------------------------------------------------------------- | ---------------------------------------- | -------------------------------------------------- |
| 壳 | split_in_chunks → build_chunks → build_execution_graph            | full_trace + 权重                        | **主机喂**开头（只喂一次表）                 |
| A  | to_field / pad / into_mle + gen_lookup_witness → batch_commit      | witness ∇；**MSM 基点=g1_powers** | 内部（纯数据变形）+**SRS 预置**              |
| B  | evals(point) + EQ + 挑战采样                                        | 节点输出 ∇                              | 内部 + 挑战内建 transcript                         |
| C  | op.prove（fix_axes→虚拟多项式→逐轮 sumcheck；查表层走 LogUp GKR） | 节点 ∇+claims                           | 内部 + 挑战内建                                    |
| D  | claims_batching_sumcheck + linear_combination + HyperKZG::open      | 全部 claims；EQ 的预置承诺；rlc_poly     | 内部 + 挑战内建 + 依赖预置承诺 +**SRS 预置** |
| 壳 | CanonicalSerialize + postcard                                       | 各段 Proof 字段                          | 内部；**Proof 交主机**                       |

> **★ 只做 A→B→C→D 四段能否完整跑通一次 prove？——能，且正好闭环**（核实自 `iop/prover.rs:995/542/837`、`poly_commit/context.rs:149-210`、dp `arkyper/mod.rs:329-383`）：
>
> - **内部自足**：i64→Fr / pad / into_mle、MSM 切块(2^20/128 是代码常量)、witness 生成、逐节点 sumcheck、RLC、折叠、kzg_open_batch、序列化、以及全链 B/C/D 的 **Blake3 挑战采样（最易漏的内建件**——挑战若由主机出，A/B/C/D 就不自足、会逐轮爆炸）。
> - **必须 setup 预置（随 s 生成，提前装载）**：`SRS/g1_powers`（尺寸 `1<<max_num_vars`，**与 s 绑定**，`context.rs:166`）、`model_commitments`（含表承诺，`context.rs:210`）。A/D 的 MSM 基点、D 的 EQ 都靠它们。
> - **主机只在首末**：喂 `full_trace+权重`，收 `Proof`（壳·序列化）。
> - **求逆在主链非必需**：各 provable 层的 shift 用乘 `lt_eval` 处理（`generic_prove.rs:244-254`），全程无 `inv()`；仅 BatchFFT 旁路用两处 inverse（`prover.rs:345/474`）。

---

## 2. 壳 · 入口 / 切块 / 调度

**干什么 / 在哪**：把"整份 trace + 模型图"切成若干 chunk、建执行图、做调度，把证明分派到 `prove_chunk`。**本壳只做数据切分/组装/调度，不做任何域运算**——是 `prove_chunk` 躲在外面的壳。

- **入口链**：§1.1 全树已给。记住一句话：**壳只切分/调度，prove_chunk 才是计算体**——`llm.rs:317 → Driver::prove(model/llm.rs:774) → chunked_prove_local(prover.rs:995) → build_local_execution_graph(prover.rs:891) → run_execution_graph(prover.rs:962) → SequentialExecutor::run(scheduler.rs:161) → ReadyNode::run(scheduler.rs:257) → node.run → ProverGraphNode::run(prover_graph.rs:157) → ChunkProver 分支 → prover.prove_chunk(prover_graph.rs:203)`。**此后 A→B→C→D 四段全在 prove_chunk 内（prover.rs:542），到最末又有序列化壳。**
- **入口层**（壳·入口）只做「切块 + 组装执行图 + 调度」，**不做任何 sumcheck/承诺**。
- **per-chunk 层**（内段 A→B→C→D）才是真正的证明计算：对每个 chunk 逐节点跑 `prove_chunk`。

**操作**：`split_in_chunks`(iop/prover.rs:1019) 选切块策略 → `ModelChunk::build_chunks`(chunking.rs:267) 按拓扑切模型图成子图+边界边+替换节点 → `replace_splitted_nodes`(prover.rs:1021) 标记被切节点 → `to_verifier_io`(prover.rs:1023) 域化公共 IO → `build_local_execution_graph`(prover.rs:891/918-936) 建执行图 → `run_execution_graph`(prover.rs:962) 交给 `GraphScheduler`+`SequentialExecutor`(scheduler.rs:161) 做**调度**（只发信号、不着证明计算）。**只切表、不碰数值**：full_trace 按 chunk 子集选出，每块仍是 `HashMap<NodeId,Step>`，Step 里是 `Vec<TensorHandle>`(i64 句柄)。**数据搬运/子集选取，零域运算。**

**多 chunk → Final 合并**：执行图有 `C+2` 个节点：

```
ProverSplit ──chunk_trace_i──▶ ChunkProver(1..C)  每块各自 prove_chunk(A→B→C→D) 产出一个 ChunkProof
      各 ChunkProof ──────────▶ Final  合并成单个完整 Proof
```

- `ProverGraphNode::run`(prover_graph.rs:157) 是**调度真正落地的地方**——一个 `match self` 按图节点类型分派三路：`ProverSplit`(切 chunk)、`ChunkProver`(触达 `prove_chunk`(prover_graph.rs:203))、`Final`(收集 ChunkProof 合并成 FinalProof)。
- 默认 `num_chunks=None/Some(1)` 时只有 1 块，ProverSplit 不切、Final 直接收；`C>1` 才走并行聚合链。**对 FPGA**：块数 C = 端到端内核的并行度上限。

> **概念落地**（图·节点·端口·子图·执行图·拓扑）：
>
> - **模型图** = 一个 `Node::Inner(L)` 一个模型算子，描述整个网络，一次建好。
> - **子图** = `ModelChunk.subgraph`（`ChunkedGraph`），从大图按层割出的拼图；incoming/outgoing 是"源/目标在别的 chunk 的边"。
> - **执行图** = 描述"一次 prove 的调度步骤"的另一张图（node0 Split、ChunkProver、Final）。**模型图告诉你"网络长什么样"；执行图告诉你"证明怎么分派几步跑"**。
> - **逆拓扑** `backward_iter`：从输出往输入，每个节点在后继访问完才出现——必须逆序，因证明一层 = 在其输出张量上立 claim、再把 claim 当输入 claim 回传前驱。

---

## 3. 段A · witness 域化 + 批量承诺

**这一段到底在干嘛**：给 `prove_chunk` 预备"可被证明的数据底座"。模型图里每个**走查表**的节点，其输入/输出中间量刚从前向推理以 **i64** 存进 trace；要拿它们做零知识证明，必须先 (1) 域化 `i64→Fr`、补 2 的幂、重解释成**多线性 MLE 求值表 ∇**，再 (2) 把全图 witness ∇ **一次性**批量承诺（KZG/MSM），给每条 ∇ 一个"防伪承诺" G1 点。**承诺完成之后，∇ 才算"定了型、可追溯"**——后续 B/C/D 的所有断言都落在这批已承诺的 ∇ 上。整个 A 在 `prove_chunk` 最前一行 `instantiate_witness_ctx_for_chunk`(:562) 里一次做完，**每个 chunk 一次**。

段 A 内分两步，用 `3.1 / 3.2` 标，是段内的两个动作、**不是新段**：

### 3.1 域化与算子化：i64 → Fr → MLE（数据变形）

**做什么 / 在哪**：把 trace 里存的 i64 中间量转成 Fr、补到 2 的幂、再重解释成多线性扩展（MLE 求值表）。这是"i64 数值流通证 → Fr/∇"的**唯一一次域化**，之后全程在 F / G1 上流转。**也是 prove 里最贴 trace 的一次大规模搬运+转换**。

**逐节点固定三步**（源码 `quantization/mod.rs:263`、`tensor/mod.rs:812/400`）：

```
(a) to_field()：i64→F  逐元素，负 i64 归约 mod p         # quantization/mod.rs:263
(b) pad_next_power_of_two()：尾部补 0 到 2^k             # tensor/mod.rs:812
(c) into_mle()：行主序重解释为超立方体求值表              # tensor/mod.rs:400
```

- 读代码：主循环里就是 `output_tensors().to_field().pad_next_power_of_two()`（见 C）。
- 数据：`Element=i64`(8B) → `Fr`(32B)，形状不变 → 补 0（记录 unpadded_shape）→ ∇。**就地 repack，大多是数据搬运不是算术**。

**MLE 是什么存储、什么序**：一个张量 `[s,768]` 域化后补到 `[s',768]` 使总元素数 = `2^k`，`into_mle()` 把它逐元素平铺成一个 `DensePolynomial`：长 `2^k` 的数组，下标 = 布尔超立方体 `{0,1}^k` 的行主序编码。下标 `idx = b₀+2·b₁+…+2^{k-1}·b_{k-1}` 对应求值 `ℳ(b₀,…,b_{k-1})`；`num_vars=k`。**这是求值形式**（每项 = 一个输入张量的整数转域）。理解后续 sumcheck"折叠"的关键：把数组当超立方体的表。

> **FPGA**：i64→Fr 最省 `|i64|` 常数模归约 + 符号位补 p；pad=搬移+补 0；into_mle=无拷贝重解释（连续内存则零运算）。

### 3.2 witness 批量承诺（MSM · 重量级）

**做什么 / 在哪**：把每个走 lookup 的节点的 witness MLE（查表的输入/输出域化结果）+ 查表多重度 + chunk 边界多项式，**一次性**批量承诺。**每个 chunk 最重的一步（≈4% Global）**，源码落点 `generate_lookup_witness_for_chunk` → `batch_commit`。

**操作**（lookup/context.rs:224-420）：

- ① 并行逐节点 `gen_lookup_witness`(context.rs:256)：对 GELU/softmax/norm/logits 从 step 产 witness MLE（如 GELU 的 input_mle+output_mle）+ `element_counts`；
- ② `multiplicities`(context.rs:313)：`mult_i = 被查次数 ÷ 表出现次数`（LogUp 输入）；
- ③ `all_polys` 拼一个 Vec **一次性** `batch_commit`(context.rs:420)。

**批量切块与 MSM**（poly_commit/context.rs:58-115）：

```
num_chunks_per_poly = ceil(2^poly_vars / 2^20).min(128)   # >2^20 才切，上限 128
每块 = 一次 size-2^20 Pippenger MSM: C = Σ aᵢ·(g^{τ^ᵢ})  (arkyper/mod.rs:471 batch_poly_msm)
MSM 块数总 = Σ_polys ceil(2^poly_vars/2^20) = ΣQ/2^20    # ΣQ = trace 域化元素总量
```

读代码：**整个 A 的入口 `instantiate_witness_ctx_for_chunk`(iop/prover.rs:1114) → `generate_lookup_witness_for_chunk`(lookup/context.rs:224) → 全 chunk 一次 `batch_commit`(lookup/context.rs:420 / poly_commit/context.rs:325)**。返回值 `chunk_commitments: Vec<CommittedPolynomial>`（每个 witness 多项式的承诺集，每 2^20 块一个 G1 点）。

> **★ 分清两类承诺（别混）**：
>
> | 承诺类型                                  | 何时                                        | 内容                                                                                                                                         | 源码                               |
> | ----------------------------------------- | ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- |
> | **模型/表承诺（setup 期，一次性）** | `generate_contexts` 构建 ProverContext 时 | 各层固有权重 MLE（model_polys）+ lookup 查询表`committed_columns`（如 GELU 表输出列），走一次 `batch_commit`，表用独占 `table_node_id` | `poly_commit/context.rs:170-214` |
> | **witness 承诺（每 chunk）**        | prove_chunk 每块开头                        | 逐节点输入/中间输出 MLE + lookup multiplicity                                                                                                | `lookup/context.rs:224`          |
>
> 模型/表承诺**不进 chunk、不随 s 变**，全局共享；段 A 的 `batch_commit` 只覆盖 witness 层。**FPGA**：表承诺是少量固定 G1 点，可预置，不计入 per-prove MSM。

> **为何 MSM 次数少、sumcheck 次数多**：MSM 是"批量化、块化"的（一次调用吞全部 witness），次数由**域化元素总量÷2^20** 定；而 sumcheck 是"逐节点颗粒"的。节点多只让 `batch_commit` 打包更多多项式，不增加调用数。

---

## 4. 段B · 播种 claims

**这一段到底在干嘛**：为逆拓扑 sumcheck 播种种子。证明是先算**输出侧 claim** + 本块**外向边 claim**，逆序才走得下去——断言"MLE 在某个点上的求值"。在 `prove_chunk` 主循环前（prover.rs:583-621 + 627-666）。

**做法**（两小块）：

- ①`output_claims_by_port`：取输出边 source 节点 step → `output_tensor_at` → `to_field().pad_next_power_of_two()` → `compute_claim(transcript, tensor)` 在该输出的 MLE 上采一点算 claim；
- ②`chunk_output_claims`：取外向边源 tensor → `into_mle()` → **transcript 采共同求值点 `common_point`**（一点点增长，保证 Fiat-Shamir）→ `mle.evaluate(eval_point)` → Claim。

**产出**：`output_claims_by_port: HashMap<ChunkedOutput, Claim>`（每个模型输出端口 → 求和 claim）+ `chunk_output_claims: HashMap<NodeOutput, Claim>`（本块输出、别的块要用的 claim）。`Claim{ eval_point: Vec<F>, eval: F }`。

> **claim 是"逆序的货币"**：一个节点既是一层输出又是下一层输入。sumcheck 在节点 A 输出上给"B 需用的某点求值 claim"，这 claim 回传给 A 当**输入 claim**；逆序保证后层先有 claim、前层拿到继续证自己的输入。**链从模型输出端逆着闭合回输入端**——这就是"必须等推理全完成才可 prove"的代码根因（:178 注释）。
>
> **末 token 分支**（H 段4.0 / F §3）：LLM 只证明最后一行 logits，`final_proj→argmax` 的 argmax 值分开参与证明，其余 `s-1` 行 logits **不进 sumcheck、不承诺**。vocab(50257) 宽那一次 logits 是常数规模的单行粒度，不随 s 放大。

> **FPGA / 是否该单独加速**：B 极轻（~0.3%），本质是"采样+求值"。**不建议把它当独立硬件段**——它是 C 逆拓扑循环的播种前拍，跟 C 在同一时序链上。做硬件时把 B 的"采点+求值"合进 C 的前置逻辑即可。

---

## 5. 段C · 节点级 sumcheck

**这一段到底在干嘛**：逆拓扑逐节点证明"节点的输出 = 输入做该运算"。**sumcheck 主耗时的落点**（实例数≈可证明节点数 V），源码落点 `backward_iter` 主循环里的 `op.prove`。**这是四个段里最大的一头（~84%），硬件的主战场。**

**主循环（prover.rs:671）逐节点的动作**：

- ①`get_step` 取节点 step；
- ② `output_tensors().to_field().pad_next_power_of_two()` 域化该节点输出（**域化在这里再次出现——随用随转**）；
- ③ `claims_for_node` 收集下游给本节点的 claim；
- ④ `flatten_and_merge_claims`(= `merge_claims`，same_poly::Prover，prover.rs:1103) 把指向同一多项式的多 claim 合成单点；
- ⑤ `op.is_provable()` 才 `op.prove`（立 sumcheck 证 "输出=输入运算"，并把结果转成本节点**输入**端口 claim 回传）；非可证明层（split/recombination）只透传；
- ⑥ `claims.extend(... NodeInput::new(node_id,i))` 存输入 claim 供更上游取。

**读代码逐函数**：

| 调用                                                        | 做什么                                       |
| ----------------------------------------------------------- | -------------------------------------------- |
| `chunk_trace.get_step(node_id)`                           | 取该节点中间量                               |
| `.to_field().pad_next_power_of_two()`                     | **i64→Fr+补 2 幂**（域化逐节点落点）  |
| `claims_for_node(node_id, &claims, &chunk_output_claims)` | 收集"本节点输出端口"被哪些下游引用           |
| `flatten_and_merge_claims(...)`                           | 同输出端口多 claim → 合成"该多项式单点取值" |
| `op.is_provable()`/`op.prove(...)`                      | 可证明层立 sumcheck 证关系；非可证明层透传   |
| `claims.extend(...NodeInput::new)`                        | 按输入端口号存 claim，上游逆序后取           |

**每类节点的 sumcheck 实例细分**（决定 `V` 的构成）：

| 节点类型                            | 内部 sumcheck                                      | 源码                                           |
| ----------------------------------- | -------------------------------------------------- | ---------------------------------------------- |
| einsum（GEMM：qkv/c_fc/c_proj/out） | 1 主 +（LHS 多 claim 时）+1 input_agg              | einsum/prove.rs:184/288                        |
| 查表层（GELU/softmax/LN/RMS/SiLU）  | **LogUp GKR = `total_layers` 轮 + 1 链接** | logup_gkr/prover.rs:183 + generic_prove.rs:222 |
| embedding                           | 1                                                  | embeddings.rs:443                              |
| split                               | 同点→0；异点→每输入 1                            | split.rs:388/432                               |
| 非可证明层（split/recombination）   | 0（只传播 claim）                                  | iop/prover.rs:781                              |

> **`d_L` 是什么**：查表层节点用 **LogUp** 逐变量折叠证"查表+多重度"。折叠 1 变量 = 1 次 sumcheck；`total_layers = log₂(查表层张量元素数)`，只由查的表多大/量化位宽定，**不随 s 变**。每查表层 = `d_L 轮 + 1 链接 = d_L+1` 个实例（GELU d_L≈8–20，LN/RMS d_L≈8–13）。
>
> **einsum 如何转成一次 sumcheck**：`fix_axes` 把固定输入消成"求和表达式"——Outer/Stacked 轴固定成 `lhs_points/rhs_points`，**Contracted 轴（求和轴）索引被消去 → 这部分自由变量构成 `num_vars = ceil_log2(contraction_size)`**，补 0 到 2^num_vars 逐块做 DensePolynomial。于是证 `Σ_contract (LHS(j)·RHS_c(j)) = 输出`，即把 GEMM 求和转成超立方体求和。一个 einsum = 1 次主 sumcheck + 多 claim 时 1 次 input_aggregation（prove.rs:285-288）。
>
> **sumcheck 公式与复杂度**（求和论证 XZZPS §3.2）：单实例证 `Σ_{x∈{0,1}^n} g(x) = y`，跑 n 轮；第 i 轮发单变量轮多项式 `f^i(x)`（在 `d+2` 点 `x=0..d+1` 上取值），验证者采挑战 `r_i`，把 g 固定一维。轮多项式 = 对每个剩余子超立方子集，把 d 个 MLE 的乘积求和。每轮遍历 `2^{n-i}`，总域乘 `≈ n·(d+1)·d·Σ(2^n/2^i) = O(n·d²·2^n)`——即 O(节点张量总元素)。
>
> **claim 寄存器语义**：`NodeOutput` 播种的 claim 不在 `claims` 里，来自段 B 的 `output_claims_by_port`；遇到输出节点先把 `(node_id,0)` 种子塞进 claims，让上游取到"我输出应在的值"。

---

## 6. 段D · 合并 + 打开

**这一段到底在干嘛**：把本 chunk **所有已生成 claim**（node sumcheck 产物 + model 权重 claim + 表 claim）**归约到单点**，再 RLC 塌缩成**一条**多项式，最后 KZG 打开证明这条。**每 chunk 1 次**。**这是"只一次打开"的根因。**

段 D 内分两步，用 `6.1 / 6.2` 标，是段内的两个动作、**不是新段**：

### 6.1 合并 sumcheck + RLC：`commit_prover.prove`

**入口链**：prove_chunk 收尾 `commit_prover.prove`(poly_commit/prover.rs:97) → `claims_batching_sumcheck`(prover.rs:140) → `build_sumcheck_expression`(poly_commit/mod.rs:93) → 1 次合并 sumcheck → `linear_combination`(RLC) → `PCS::prove`(prover.rs:125，落到 6.2 打开)。

**做法**：

- ① 对每个 claim：`evals(poly.eq_point_per_chunk(point)).into_mle()` 构造 EQ 多项式（"只在点 p 处=1"的指示函数）+ `batching_coefficients_for_chunks`(`C_j = eq(j, point)`)；
- ② `build_sumcheck_expression`：每 (多项式,claim) 组装成 `挑战·(Σ_j C_j·P_j_chunk)·eq_p`，全项累加成**一个**虚拟多项式；
- ③ 一次 `IOPProverState::prove` 归约到单点；
- ④ `transcript` 对每个 committed poly 采随机挑战 `c`；
- ⑤ `linear_combination`(dense.rs:934)：`rlc = Σ c_i·P_i` → 1 条 rlc_poly。

**产出**：`ClaimsBatchingProof{ sumcheck_proof, sumcheck_evals, sumcheck_point, committed_polys }` + `rlc_poly`；随后 `PCS::prove` 打开（6.2）。**term 数 = Σ(每 poly 的 claims × chunks)** = "多 claim 一次归约到单点"的规模口径。

> **合并 sumcheck 的公式含义**：`eq_p(x)=∏_i(pᵢxᵢ+(1−pᵢ)(1−xᵢ))`——把"在点 p 求值 v"这个标量 claim 表达成超立方体求和；batching 系数 `C_j=eq(j, p 高位)` 把切块拼回完整点（`P(r)=Σ_j eq(j,高位)·P_j(低位)`）。
>
> **RLC → 打开**：对每个 committed poly 采随机挑战 c，`rlc=ΣcᵢPᵢ` 把"很多多项式在单点"塌缩成"**一个**多项式在单点"；随后 PCS::prove 只打开这一个 rlc。**"只一次打开"就来自 RLC 在前。**

### 6.2 KZG 打开：`HyperKZG::open`

**入口链**：`PCS::prove`(arkyper/mod.rs:497，先求值) → `HyperKZG::open`(:329) → `kzg_open_batch`(:186)。

**做法（三阶段）**：

- **Phase1 折叠**：`polys[i+1] = rᵢ·(p[2j+1]−p[2j]) + p[2j]`，逐变量把 `2^m` 压到常数（每轮减半，`ell−1` 轮），轮间串行/轮内并行（arkyper/mod.rs:349-356）。
- **Phase2**：`msm::batch_poly_msm(pk.g1_powers, &polys[1..])` 承诺 `ell−1` 个折叠中间多项式 → transcript 采 `r` → 构造 `u=[r,−r,r²]`（3 个特殊打开点，:373）。
- **Phase3 `kzg_open_batch`**(:186)：`v[i][j]=f_i(u_j)=eval_as_univariate` → `q_powers = challenge_scalar_powers` → 对 `ell` 个折叠多项式**再 RLC** 按 `q_powers` 组合成 `b_poly`（:216）→ `kzg_batch_open_no_rem(&b_poly, u)`(:222) 对 B 在 3 点打开。

**产出**：`OpeningProof`：`w=(w₀,w₁,w₂)` 三个 G1 witness + `v`（各折叠在 3 点的求值）。**打开总 MSM ≈ Phase2 的 ell−1 折叠 + Phase3 的 3 点 open（≈承诺 ×4）**。

> **为何 3 点 u=[r,−r,r²]**：HyperKZG 展开三点求值让验证者用**一个 pairing 等式**验 3 个求值（由 `d₀=d₁²` 二次关系联动），比逐点开省 N−1 次 pairing。
>
> **为何 open 里又 linear_combination**：把 ell 个折叠**再**按挑战线组合成 B，一次 3 点打开省掉重复。
>
> **串行依赖链**（第 i 轮折叠依赖第 i−1 轮）对流水不友好。

> **FPGA / 是否该切分**：段 D 是全链**唯一物理上可分开的段**——6.2 的 KZG 打开是独立 MSM 引擎，可与段 A 的批量承诺 MSM 共用一块引擎（[附录 F](#附录-f--四段分割合理性--实测核验) 详议）。段 D 整体 ~10%。

---

## 7. 壳 · 序列化 / 证明打包

**干什么 / 在哪**：把 OpeningProof 与各 ChunkProof（含 TableProof）打成一个完整 `Proof` 字节流交给 verifier。**纯本地、无算术。** 入口链：`Final.run` → `run_execution_graph` 汇集各 ChunkProof → `iop/mod.rs` Proof 结构 → postcard 序列化（提交 187d6c9b）+ ark_serialize。

**做法**：`CanonicalSerialize` 压缩：Fr→32B LE、G1→(x,y) 各 32B（∞ 哨兵）；postcard 把各段 proof 结构扁平化/拼接成连续字节。喂进 `iok`：各 producer 段的 Proof 字段——`ChunkProof`(iop/mod.rs:67)、`TableProof`(:138)、`OpeningProof`(poly_commit/mod.rs:155)、output_evals 等。

> **FPGA**：域→字节打包（bus 编解码）+ proof 结构顺序拼装（长度前缀）；无算术。

---

## 8. 贯穿底座与关键数据结构

下面是 A/B/C/D 都要打交道的数据结构，FPGA 规划内存/布局前先读这一张表。设计意图一句话：**证明的输入是"图 + Trace 表"，不是逐 token 状态。**

| 结构                     | 定义点                  | 内容                                                                                 | 设计意图                                                                             |
| ------------------------ | ----------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| `Trace<Element>`       | `model/trace.rs:29`   | `steps: HashMap<NodeId,Step>` + 输入/输出句柄 + 可选 split 信息                    | 一次证明的**全部输入**；`Element=i64`(8B)                                    |
| `Step<Element>`        | `model/trace.rs:388`  | `node_inputs: Vec<TensorHandle>` + `node_outputs: NodeOut`(outputs+proving_data) | 每个图节点的**端口级**快照；proving_data 存 LN mean/std、softmax shift、argmax |
| `ModelChunk`           | `iop/chunking.rs:145` | 子图`ChunkedGraph` + incoming/outgoing 边 + replaced_nodes                         | 把大模型拆成可分布式/局部 prove 的块                                                 |
| `ProverContext<F,PCS>` | `iop/context.rs:36`   | PCS 参数 + 承诺上下文 + lookup 上下文                                                | **Setup 产物**，持全部已承诺权重及其承诺（静态，每次 prove 复用）              |
| `ChunkedCommitment`    | `poly_commit/mod.rs`  | 承诺多项式的块化视图 + 逐块求值/承诺                                                 | >2^20 元素 MLE 切成 2^20 块，按块做 Pippenger MSM                                    |
| 执行图`Graph`          | `iop/prover.rs:906`   | Split + C×ChunkProver + Final                                                       | 把 prove 调用序列建模成可调度图，支持多 chunk 并行                                   |

---

## 附录 A · 调用次数与规模公式（FPGA 调度口径）

供"每步该拉几次引擎、每次吃多少"。设 `C`=chunk 数（默认 1），`V`=可证明节点数，`ΣQ`=域化元素总量。

| 算子（段）                   | 调用次数                      | 随什么变         | 规模                                                      |
| ---------------------------- | ----------------------------- | ---------------- | --------------------------------------------------------- |
| batch_commit（witness，段A） | `1 次/chunk = C`            | 不变             | 内部按 2^20 切`≈ΣQ/2^20` 个 Pippenger MSM（∝ s+s²） |
| model 承诺（段A setup）      | `1 次/整模型`（Setup 静态） | 不变             | 权重元素数/2^20 个 MSM                                    |
| 播种 claims（段B）           | `≤ 输出/外向边数 /chunk`   | 不随 s           | 每边 1 claim：一次求值 + EQ                               |
| 节点级 sumcheck（段C）       | `≈ V 次/chunk`             | **不随 s** | 每实例 num_vars 轮，每轮 O(2^余×多项式×次数)            |
| 合并 sumcheck（段D）         | `1 次/chunk = C`            | 不变             | 变量数=chunk 最大 num_vars；term 数=Σ(claims×chunks)    |
| PCS 打开（段D）              | `1 次/chunk = C`            | 不变             | ≈ 3n MSM + 折叠（随 ΣQ）                                |

**两条规律**：

> - **MSM 块数** = `ΣQ/2^20`，随 trace（∝ s + s²）涨；**不由节点数定**。
> - **sumcheck 实例数** = 节点数 `V`，**几乎不随 s 变**，但**每个实例的规模随 s² 的 score 张量增大**。对 FPGA：延长上下文更多是"同一个块流水线多吃数据"，不是"多拉起算子"。
> - **重计算算子 = 3 类**：批量 MSM、sumcheck+EQ/fold。**但绝不能只做这 3 个**——域化、转 MLE、RLC、挑战采样等配套算子若留主机，会退化成**逐轮交互爆炸（数千次小包往返）**。正确形态是"端到端 prove 内核"，主机只在首尾喂 trace+权重/收 Proof，挑战采样贴近/下放 FPGA。

**内存扩展与量级公式**（更大权重模型）：

```
权重内存  W_bytes = params × 32B(Fr)            # 静态，一次 Setup
trace 内存 Q_bytes = ΣQ × 8B(i64)               # = F §4
score 项  S     = Hq × s² × 8B                 # 注意力 score 二次项，长上下文主增长
trace_Q(s) ≈ α·s + β·s²                        # GPT-2: α=1.29MB, β=2.3KB (i64)
```

| 模型          | 权重(Fr,32B) | trace(i64) s=512 | trace(i64) s=1024 |
| ------------- | ------------ | ---------------- | ----------------- |
| GPT-2 124M    | ≈ 3.97 GB   | ≈ 1.3 GB        | ≈ 3.7 GB         |
| GPT-2 XL 1.5B | ≈ 50 GB     | ≈ 6 GB*         | ≈ 14 GB*         |
| Llama-2-7b    | ≈ 214 GB    | ≈ 39 GB*        | ≈ 94 GB*         |
| Gemma-3-270M  | ≈ 8.6 GB    | ≈ 4.3 GB*       | ≈ 10.4 GB*       |

> *(带 `*` 为按 F §5 系数外推估算，仅 GPT-2 一行是逐节点核实锚点。)* **判定**：单板 U280-HBM(~4.6GB) 上 GPT-2 权重(Fr)已贴 HBM；7b 权重 214GB 远超单板→分卷/多板。**内存天花板在权重(Fr)**，不在 trace(i64)，除非 s 极大（score 二次项追上）。

---

## 附录 B · 全流程需实现的算子清单（FPGA 完整边界）

> 要"FPGA 全流程加速都要实现哪些算子"，prove 路径从头到尾**存在的每个算子**。重计算算子是批量 MSM、sumcheck、EQ/fold；但**全流程还缺 3 类配套算子，缺了就会回到逐轮交互爆炸**。

| #  | 算子                                                           | 段   | 输入 → 输出                         | 数据搬运                         | 若留主机 → 交互                   |
| -- | -------------------------------------------------------------- | ---- | ------------------------------------ | -------------------------------- | ---------------------------------- |
| 1  | **i64→Fr 域化** `to_field`/`pad`                    | A/C  | trace(i64 8B) → Fr MLE(32B)         | trace 大批量写(∝ΣQ)            | 整份 trace 要搬，仍可流式          |
| 2  | **张量→MLE** `into_mle`                               | A/C  | Fr 张量 → 超立方体表                | 就地/搬入                        | 需先域化后生成                     |
| 3  | **★ EQ 求值** `evals(point)`                          | B/D  | 点 → EQ 2^k 表                      | 中量                             | 每 claim 生成一次                  |
| 4  | **★ 批量承诺** `batch_commit`                         | A    | Σ 个 2^20-chunk MLE → 承诺         | **大批量流式(32MB/chunk)** | **主计算**                   |
| 5  | **★ 节点级 sumcheck** `IOPProverState::prove`         | C    | 虚拟多项式 → sumcheck proof+claims  | 逐轮                             | **主计算 + 逐轮**            |
| 6  | **★ 合并 sumcheck**（含 `build_sumcheck_expression`） | D    | 全部 witness+model+EQ → 单点        | 逐轮                             | 主计算 + 逐轮                      |
| 7  | **RLC 线性组合** `linear_combination`                  | D    | Σ committed polys × 挑战 → 1 poly | 读全部已承诺块                   | 面对大 witness 也要搬一遍          |
| 8  | **折叠 fold**（open Phase1）                             | D    | 多项式 → k-1 轮折叠                 | 逐轮                             | 逐轮                               |
| 9  | **★ 打开 MSM**（≈3n）`PCS::prove`                    | D    | 折叠后 → 打开证明                   | **大批量 MSM**             | 主计算                             |
| 10 | **LogUp/GKR**（仅 lookup 节点）                          | C    | lookup 表+multiplicity → GKR        | 中等                             | 多一块引擎                         |
| 11 | **挑战采样/transcript**`(Blake3)`                      | 全程 | 消息 → 随机挑战                     | 小量但高频                       | **最关键：逐轮，须贴近引擎** |
| 12 | **状态编排**（claims map、proof 组装）                   | 全程 | flags                                | 少量                             | 可留主机但需 chunk 级同步          |

> **重计算 = 3 类**（批量 MSM、sumcheck、EQ/fold）；**必配配套 5 类不可缺**（域化、转 MLE、EQ 生成、RLC、挑战采样）。FPGA 要做一个**端到端 prove 内核**——主机只喂 trace+权重，板内完成 1→12，只在首尾/边界交互。**仅做 MSM+sumcheck 而把配套留主机，会退化成逐轮爆炸。**

---

## 附录 C · FPGA 阶段化加速方案

| 阶段化                                         | 覆盖 prove 段           | 加速比潜力         | 难度           | 说明                                               |
| ---------------------------------------------- | ----------------------- | ------------------ | -------------- | -------------------------------------------------- |
| **M0：仅 MSM 引擎（段A/D 的 MSM 部分）** | witness 批量承诺 + 打开 | 承诺+打开 ~60–90% | ★★★ 中      | 一个 Pippenger 引擎，块=2^20；需 HBM 喂 32MB/chunk |
| **M1 = M0 + sumcheck 引擎**              | 内段 A→B→C→D 全流程  | sumcheck +35%      | ★★★★ 中-高 | 要一块 sumcheck 多轮引擎(EQ/fold)，块级 handler    |
| **M2：数据通路 + 双缓冲乒乓**            | 全部                    | 消除搬运瓶颈       | 中             | 决定吃满 MSM/sumcheck 的前提                       |

> **系统划分落在 M1**（两引擎 + 数据通路）最能兑现"全流程加速"。所有算子最终收敛到"MSM 引擎 + sumcheck 引擎 + 数据编排"，调用次数收敛为 `C` + Setup 权重一次。逐节点 sumcheck 不是"逐个拉起"，而是**把整块接进 sumcheck 引擎逐 chunk 流式处理**。

**关键风险/待求证点**：

1. **s² score 二次项**是"整体物化+承诺"还是"动态求值"——决定长上下文能否剪裁。
2. **单板内存天花板在权重(Fr)**：7b 权重 214GB 需分卷/多板；目标更大则单板不成立。
3. **sumcheck 逐节点实例多但有随机挑战依赖**：每轮依赖上一轮随机点；node-level 可并行（不同节点独立），同节点轮次串行——并行度在"节点间并行 × 节点内流水"。
4. **块粒度 2^20(32MB-Fr/chunk)** 是唯一 I/O 通吃粒度，也是 FPGA 双缓冲乒乓建议块。

---

## 附录 D · 算子 → 源码对照表（FPGA 对着查）

| 算子                                           | zkml 调用点                                            | dp-crypto 实现                                |
| ---------------------------------------------- | ------------------------------------------------------ | --------------------------------------------- |
| `i64→Fr`（负归 mod p）                      | `quantization/mod.rs:263`，`tensor/mod.rs:328/389` | —                                            |
| pad 到 2^k                                     | `tensor/mod.rs:512/812`                              | —                                            |
| 张量→MLE`into_mle`                          | `tensor/mod.rs:400`                                  | `poly/dense.rs`                             |
| EQ 求值`evals(p)`                            | `poly_commit/prover.rs:172/201`                      | `poly/eq.rs:18`                             |
| 批量承诺`batch_commit`（Pippenger，块 2^20） | `poly_commit/context.rs:58/325`                      | `arkyper/mod.rs:471`                        |
| 节点级 sumcheck`IOPProverState::prove`       | `iop/prover.rs:770`→`layers/*/prove.rs:184`       | `sumcheck/prover.rs:116`                    |
| 合并 sumcheck（`build_sumcheck_expression`） | `poly_commit/prover.rs:140`、`mod.rs:93`           | `sumcheck/prover.rs:116`                    |
| RLC`linear_combination`                      | —                                                     | `poly/dense.rs:934`、`arkyper/mod.rs:216` |
| 折叠（open Phase1 串行递推）                   | —                                                     | `arkyper/mod.rs:349-356`                    |
| 打开 MSM（3 点 kzg batch）                     | `poly_commit/prover.rs:125`                          | `arkyper/mod.rs:362,376,186`                |
| 挑战采样（Blake3 Fiat-Shamir）                 | `iop/prover.rs:158` 等                               | `arkyper/transcript`                        |

---

## 附录 E · 吃透顺序（逆向看代码）

1. 先 `<F5>` `iop/prover.rs:1040` → `:995`，看**壳·入口**怎么从整份 trace 切块建图。
2. 主干进 `:542 prove_chunk`，盯 **`:671 backward_iter` 主循环**（段 A/B/C 的骨架）。
3. 每节点跳 `layers/einsum/prove.rs:45`，体会"一次 sumcheck 证明一个 GEMM"（段C）。
4. 看 `lookup/context.rs:420` 一行，明白 witness 是 **per chunk 一次批量承诺**（段A）。
5. 看 `poly_commit/prover.rs:97/140`，懂"所有 claim → 一个合并 sumcheck → RLC → 一次打开"（段D）。
6. 最后到 `arkyper/mod.rs:329`（折叠）+ `:186`（3 点 open）看**最底层叶片**（段D）。

---

## 附录 F · 四段分割合理性 + 实测核验（一体化 prove 内核,非四分板）

> **定位**：正文第 3~6 章把 `prove_chunk` 拆成 A/B/C/D 四段，那只是**代码阅读 / 数据流**视角。本附录回答"这四段能否作为多 FPGA 的**分割边界**"，依据两条独立证据链：① 日志实测时序（`zkml/gpt2_128/512/1024.log`，`zkml/tools/extract_prove_timing.py` 还原）；② 数据传输逻辑（§F.3）。**结论：四段不能四分板，必须做成同一套板上的端到端 prove 内核；真正在主机两侧的只有壳（入口/切块/调度 + 序列化）。唯一可选物理切点 = 段D 的 KZG 打开。**

### F.1 一页结论

| 项                                    | 结论                                                                                                        |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| 四段作为**代码阅读/数据流视角** | 合理且必要——正文第 3~6 章借此讲清 A→B→C→D 主链                                                         |
| 四段作为**硬件切分边界**        | **不合理**。四段共享 ∇ 求值表 + 挑战单链，必须拧成**一个端到端内核**留同一套板；主机侧只有"壳" |
| 跨段最大数据瓶颈                      | **MLE(∇) 求值表**（trace 域化后 i64 8B→Fr 32B），s=1024 ≈3.7GB，横跨 A/B/C/D                       |
| 挑战采样（Fiat-Shamir）               | **必须内建下放**，否则逐轮 sumcheck 退化成数千次小包往返                                              |
| 最该硬件加速                          | **C** 节点级 sumcheck(~84%)；**A/D 的 MSM**（批量承诺 / KZG 打开，Pippenger 可共用引擎）        |
| 最不该单独分段                        | **B** 播种 claims(~0.3%，是 C 逆拓扑循环的"播种前拍")；**域化**（随用随转，非独立段）           |

### F.2 实测：四段耗时占比（s=512 / s=1024）

| 里程碑                                       | s=512                     | s=1024                     | 对应段                                |
| -------------------------------------------- | ------------------------- | -------------------------- | ------------------------------------- |
| Witness commitments（域化+批量承诺）         | 15.24s (4.0%)             | 33.53s (4.3%)              | **A**                           |
| Challenge storage（挑战存储）                | 333ms (0.1%)              | 532ms (0.1%)               | **B** 一部分                    |
| **Node-level sumcheck(含播种) 主循环** | **323.46s (84.2%)** | **669.37s (85.1%)**  | **B(播种)+C(sumcheck)，C 主导** |
| Lookup Table claims generation               | 933ms (0.2%)              | 1.19s (0.2%)               | C 查表层                              |
| Generate proof（合并+RLC+KZG 打开）          | 40.62s (10.6%)            | 73.82s (9.4%)              | **D**                           |
| **Global**                             | **384.13s**         | **786.61s (2.05×)** | 全程                                  |

> s 翻倍 → Global 2.05×，与 trace∝s+s² 的二次项特征吻合（§F.6）。**计时口径**：`Node-level prover 主循环 ~84%`实际是 **B(播种)+C(sumcheck) 同体**量进同一个计时圈（`prove_chunk` 里 `output_claims_by_port`(:583)/`chunk_output_claims`(:627) 播种 + `backward_iter`→`op.prove`(:671-808) 节点 sumcheck），C 占绝对主导；B 只是它的"播种前拍"，可与 C 合并看≈84%。段A 的域化没有独立计时点，`Witness commitments`把「域化+承诺」整体量进段A。

**C 段节点级 sumcheck 按算子（s=1024，配对 sumcheck 合计 ~663s）**：

| 类别            | 次数 |    耗时 |  占比 |
| --------------- | ---: | ------: | ----: |
| Softmax         |   12 | 214.50s | 32.4% |
| Requant(重量化) |   98 | 181.47s | 27.4% |
| GeLU            |   12 |  67.81s | 10.2% |
| EinSum(矩阵乘)  |   73 |  64.29s |  9.7% |
| CasualMask      |   12 |  47.92s |  7.2% |
| Logits(输出)    |    1 |  44.53s |  6.7% |
| LayerNorm       |   25 |  37.35s |  5.6% |
| Add(残差)       |   24 |   4.57s |  0.7% |

> **判读**：**Softmax / CasualMask / attn-score 这一"注意力二次项"是 s 涨的扩音器**（s=512→1024，Softmax 214s、CasualMask 47.9s，都按 s² 长）；EinSum 次数最多(73次)但被权重固定维封顶；Logits 单节点单次最大(44.5s)是孤立峰值。→ **长上下文下，C 段里最该针对 s² 剪裁的就是注意力二次项。**

### F.3 跨段数据量——两处"切开必爆"

| 跨越         | 要传的数据                    | 可切性                                 |
| ------------ | ----------------------------- | -------------------------------------- |
| 壳入口→A    | full_trace + 权重             | 一次性搬，**可切**（只喂一次）   |
| A→B         | ∇ 表（≈3.7GB@s=1024）+ 挑战 | **瓶颈**——∇ 表 B/C/D 都要读   |
| B→C         | claims + 逐轮挑战             | **不可切**——逐轮高频往返       |
| C→D         | all claims                    | 可传一次，但同板更优                   |
| D 内 C→打开 | rlc_poly + 单点               | **可切**——KZG 打开独立成板可行 |
| D→壳序列化  | OpeningProof                  | 可切，序列化本来在主机侧               |

两处**"切开必爆"**边界：**① ∇ 表跨 A/B/C/D 复用**——B/C/D 物理上都操作 A 产出的同一批 ∇，分开放板则每块板都要持/搬那张 3.7GB 表（A→B 搬一次已是极限，此后同板复用更优）；**② 挑战单链**——sumcheck 每轮依赖上一轮随机点（正文附录 B 关键项），挑战若留主机，"立 claim→主机采样→发回→证下一轮"构成数千次小包往返，吞吐被延迟打死。

### F.4 判定：主机 壳 + 一个端到端 A→B→C→D 内核

```
┌ 主机(壳)侧: 入口/切块/调度 + 序列化/发 Proof ────────┐
│  只喂 full_trace+权重(一次), 收 Proof                 │
└───────────────┬──────────────────────────────────────┘
                │ 一次大批搬入: 权重 Fr + ∇ 表(≈3.7GB@s=1024)
                ▼
┌ 端到端 prove 内核(同一套板) ───────────────────────┐
│ A  witness 域化+批量承诺(MSM)                      │
│ B  播种 claims(挑战采样内建)                       │
│ C  节点级 sumcheck(★主算力,~84%)                   │
│ D  合并 sumcheck+RLC+KZG 打开(MSM)                 │
│    挑战采样(Blake3 Fiat-Shamir) 全内建             │
└──────────────────────────────────────────────────┘
```

- **A/B/C/D 必须同体**（§F.3 两处切开必爆）；四段耗时 = Global − 壳 ≈ **95%+**，做成一个内核后每块板只跟主机首尾交互。
- **域化不单独成段**，是 A、C 前随取随弃的纯数据变形，跟 A 放一起，不增加切分点。
- **若一定要在两块板间找唯一物理切点**，最优是 **D 的 KZG 打开**——只传小数据(rlc_poly+单点 / OpeningProof)，且其 MSM 引擎可与 A 的批量承诺**共用一块 Pippenger 引擎**（做 M0）。次优是 **A 之前**（MSM 与 sumcheck 分离，B/C/D 仍在 sumcheck 板内端到端跑）。
- **不要把四段当四块板**；真正交付是"主机 + 端到端证明内核(+可选独立 MSM 板)"。

### F.5 硬件加速优先级（按 s=1024 实测）

1. **C sumcheck（~84%）** ← 绝对大头；Softmax/Requant/注意力二次项随 s² 涨。目标一块 sumcheck 多轮引擎（EQ/fold/uni-poly），块粒度 2^20。
2. **A/D 的 MSM（~14%）** ← 引擎复用度高、独立封装简单（Pippenger，32MB/chunk），M0 起步最值，A 与 D 可共用。
3. **A 域化 + 壳入口搬入** ← 决定 I/O 是否吃满（32MB/chunk 双缓冲乒乓）。
4. **挑战采样内建** ← 不是算力但决定架构成败，缺失即逐轮爆炸。

### F.6 公式核验（s=128/512/1024 三点实测，`tools/calibrate_prove_model.py` 可复现）

**trace 数据量 ✅ `Q≈αs+βs²`**：Q(128)=203MB、Q(512)≈1.23GB、Q(1024)≈3.65GB，三点满公式；**二次项 β·s² 在 s=1024 已占 65%**。

**耗时∝数据量 ❌ 不成立**：

```
Q(1024)/Q(128) = 3733/203  = 18.4×
Global(1024)/Global(128) = 786.6/160.3 = 4.9×
→ 数据涨 18.4×，时间只涨 4.9×（Global ∝ s^0.77，下凸）
```

原因（按算子重定标）：**A 的 MSM 只吃 trace 线性部分**（实测 8.3×≈线性 8×——lookup witness 是 length-s 张量，不是 s² 的 score）；**C sumcheck 是"40% s² + 60% 固定权重/近线性"的混合**（Softmax 20.3×、CasualMask 41.7× 的 s² 分量；EinSum 仅 2.4× 被权重固定维封顶）；**D 只 3.0×**（吃 claim 数∝节点数 V≈272，不随 s）。

> **一句话**：估"某 s 的段内耗时"不能用总 trace 字节×系数；正确口径是「A 吃线性 trace、C 吃 min(节点张量, 与权重维耦合)、D 吃固定 claim 结构」，其中只有 Softmax/Mask/attn 一类随 s² 涨——直接支撑 §F.5"C 段里最该针对 s² 剪裁注意力二次项"。

> **工具**：`zkml/tools/extract_prove_timing.py`（还原日志）＋ `zkml/tools/calibrate_prove_model.py`（公式核验，纯 Python）。数据来源 `zkml/gpt2_128/512/1024.log`（GPT-2 124M, chunks=1）。口径注意：Node-level 主循环实为 B+C 同体（见 §F.2）。

---

## 附录 G · 微算子 / 密码单元 / 算子族下钻（FPGA 综合到硬件）

> **定位**：正文 3~7 章 + 附录 A~F 把 prove 拆到**算子级**。本附录把最关键的四类内容**再下钻半层到"可直接综合的硬件微操作"**，并修正两处此前来源标注。每段给「微算子 / 源码位置 / 输入→输出 / FPGA 判定」。**读者**：要落 sumcheck、MSM、EQ、transcript、以及 Lookup/Embedding/卷积/掩码这类具体层到流水级的实现者。符号沿用 `F=Fr=ark_bn254::Fr`(32B)、`G1=BN254 曲线点`。

### G.1 sumcheck 轮内微算子（dp `sumcheck_macro/src/lib.rs:62-357` + `sumcheck/prover.rs:356-383`）

> 轮内 `build_uni_poly` 由 `sumcheck_code_gen!` 宏生成。对**每个产物**（$d$ 个 MLE 的乘积），按布尔超立方体 `b` 步进 2 遍历；`v_j` = 该 MLE 的切片。

| 微算子                         | 源码位置                             | 输入→输出                     | 每轮每产物数量         | FPGA 判定                                                |
| ------------------------------ | ------------------------------------ | ------------------------------ | ---------------------- | -------------------------------------------------------- |
| **相邻差**               | 宏 `c0=v[b+1]−v[b]`               | 两相邻域元素→差               | ~len/2                 | sub                                                      |
| **差倍增**（域加非移位） | 宏 `c_j=c_{j−1}×2`               | 前差→2^t·差                  | ~len/2·⌈log2(deg)⌉  | add（自加）                                              |
| **行系数组装**           | 宏 rows                              | c 链+v[b+1]→各行系数          | len/2·(deg−1)        | add                                                      |
| **行因子跨 MLE 相乘**    | 宏 dot                               | d 个 MLE 同行因子→row-product | len/2·(deg+1)·(d−1) | **mul**                                            |
| **项系数缩放**           | prover.rs:375                        | 行值×scalar                   | deg+1                  | mul                                                      |
| 域乘幂/常数                    | 宏                                   | 补乘 2^mult                    | 偶发                   | 常数乘                                                   |
| **外推补满次**           | `sumcheck/extrapolate.rs:159-173`  | d 点值→max_degree 点值        | (maxdeg−d)·(deg+1)   | **预计算常数点积 d+1 mul+d add，运行期无 inverse** |
| **累加进轮多项式**       | prover.rs `uni_polys+=uni_variate` | 各 term→fᵢ(x)                | (deg+1)·#products     | add                                                      |
| **换维 fold(fix_low)**   | `dense.rs:477` / prover.rs:333     | MLE→固定 rᵢ 减半             | len/2                  | sub+mul+add, little-endian                               |

> **FPGA 定点**：核心是**一行宽的 256-bit Montgomery mul 阵列 + sub/add 树**。轮间**串行**（第 i 轮要第 i−1 轮挑战），轮内所有 `b` 全并行。外推在建表时消掉 inverse，**运行期不要在板上放运行时求逆**。

### G.2 底层密码单元（dp-crypto / arkworks 精确定点）

> 全程**无 NTT/FFT/小域/扩张域**——就是 **Fr 标量域算术 + G1 点算术 + Blake3**。FPGA 勿建 NTT 单元。

**G.2.1 域算术**（`ark_ff Fp256<MontBackend>`，254-bit 蒙哥马利，4×u64 肢。实为 `ark_bn254::Fr = Fp256<MontBackend<FrConfig,4>>`）

| 微算子                          | 用途               | 硬件                                                     |
| ------------------------------- | ------------------ | -------------------------------------------------------- |
| 域加（+p 条件减）               | eq/累加/fold       | 256b 加 + 条件减 p                                       |
| 域减                            | 差/折叠            | add of neg                                               |
| **域乘（蒙哥马利 CIOS）** | **吞吐瓶颈** | 256×256 Mont 乘（4-肢）；sumcheck/fold/RLC/MSM 标量共用 |
| 域取负                          | −r                | 256b 减 p                                                |
| 求和累加器（加法树）            | `.sum()`         | 流水加法树                                               |
| 求逆（Fermat p−2 幂 ~254 次）  | 见 G.2.4           | 小面积慢单元，**无需高速逆单元**                   |

**G.2.2 EQ 多项式**（`poly/eq.rs:54-141`，`virtual_poly.rs:346`）

| 微算子                                             | 每元素                       | 并行度                              |
| -------------------------------------------------- | ---------------------------- | ----------------------------------- |
| 递推展开 `evals[i]=r·scalar`，`evals[i/2]−=` | 1 mul + 1 sub                | **每层全并行，仅 n 个串行层** |
| 单点 `eq_eval: res*(xi·yi)+...`                 | 1 mul + add/sub + 1 mul 累加 | —                                  |

**G.2.3 MSM Pippenger**（**来源修正**：dp-crypto `arkyper/msm.rs` 是薄壳，CPU 直接 `VariableBaseMSM::msm`，CUDA 走 ec-gpu `MultiexpKernel`；**真正的分窗/桶算法在 arkworks `ec/src/scalar_mul/variable_base/*`**）——曲线 `y²=x³+3` over Fq（`ark_bn254::G1Affine`，COEFF_A=0,COEFF_B=3）

| 微算子                 | 说明                           | 硬件                                                   |
| ---------------------- | ------------------------------ | ------------------------------------------------------ |
| 标量→radix-c 窗口数码 | Pippenger 分窗                 | 位抽取                                                 |
| 每窗口桶累加           | 数码→bucket，G1 点 double+add | **Fq 基域点运算 + Fr 标量 mul**                  |
| 跨窗口组合             | 每窗 c 次 double + add         | 同上                                                   |
| 曲线坐标               | `y²=x³+3` over Fq          | 需 **Fq(254b) 蒙哥马利**（与 Fr 相似，两套常数） |

**G.2.4 求逆的精确口径（修正）**：**"主链无逐元素求逆"为真，但"全程无求逆"不准确**——prove 路径的求逆全为**整体/标量级**，非大张量：

| 求逆位置                                      | 是什么                                                          | 量级                         | 归属                                 |
| --------------------------------------------- | --------------------------------------------------------------- | ---------------------------- | ------------------------------------ |
| `lookup/context.rs:357`                     | lookup **multiplicity 逆**（`table_count≠1` 时）       | 每个 distinct 表值一次标量逆 | **段A witness 生成（prover）** |
| `lookup/operation/generic_prove.rs:305-309` | `inverse_lt_eval`（shift 归真）                               | 每查表层一次标量逆           | 段C 查表层（prover）                 |
| `iop/prover.rs:345/474`、`fft.rs:58/81`   | root-of-unity / 1/len 逆                                        | 每 FFT 一次                  | BatchFFT（卷积旁路）                 |
| `variant.rs:144/412`、`logits.rs:588/948` | **2⁻¹ 编译期常数逆**（折叠成 `Expression::Constant`） | 常数乘                       | circuit 构造期，非 prove 热路径      |

> **硬件结论（不变）**：这些都是每 chunk 几个标量逆 / 常数逆 / FFT 根逆，**不是 per-element** → 确实**不需要一块高速求逆单元**，一块 Fermat(p−2) 幂 ~254 次模乘的小面积慢单元（仅 BatchFFT/查表旁路/预计算表用）即够。

> **G.2.3 附 · 打开=承诺同源 MSM，负载约 4× 承诺**（承 §6.2/附录 F）：`HyperKZG::open` 的点乘总量 ≈ `(n−2)+3n ≈ 4n`，其中 3n 来自 Phase3 的 3 点 kzg_batch_open（`arkyper/mod.rs:362,376,186`）；`compute_witness_polynomial`（`mod.rs:171-183`）是串行 Horner 合成除法链 `h[i-1]=f[i]+h[i]·u`，对 FPGA 流水不友好，需窗口/SIMD 分解。

### G.3 算子族下钻（正文只"点名"、此处给硬件微拆）

#### G.3.1 Lookup 反表层（LogUp + 链接 sumcheck）— GELU/softmax/LN/rmsnorm/logits 共用

> 入口 `lookup/operation/generic_prove.rs:66 prove_lookup_op`。先把节点 i64 输入/输出域化成 witness MLE（承 §3.1/3.2），LogUp 证查表，再一个链接 sumcheck 对齐 table 输出与输入；weight 张量逐元素乘 lookup 输出。

| 微算子                      | 源码位置                     | 说明                                       |
| --------------------------- | ---------------------------- | ------------------------------------------ |
| LogUp batch prove           | `logup_gkr/prover.rs:91`   | 每层分数和恒等，多层 GKR 逐层 sumcheck     |
| 每层累加表达式              | prover.rs:224-250            | `Σ expr·Eq_poly`                       |
| power-of-two 系数缩放       | prover.rs:222                | 不同尺寸实例对齐                           |
| 低/高半合并                 | prover.rs:257                | `rk·f(r..,1)+(1−rk)·f(r..,0)`         |
| 把 lookup MLE 按 chunk 切片 | `generic_prove.rs:108`     | →lookup_polys+others                      |
| normaliser 扩维/复制        | `generic_prove.rs:183-192` | 1 值→padded_final_dim 个（数据复制/广播） |
| weight 张量重复/拼接        | `generic_prove.rs:209-219` | →second_last_dim 份（数据复制+拼接）      |
| **链接 sumcheck**     | `generic_prove.rs:222`     | lookup输出×weight=源输出                  |
| `dim_lt` 多项式求值       | `evaluate_dim_lt_poly`     | 1/lt 指示                                  |
| MLE 求值 + 除 lt            | `generic_prove.rs:247-257` | mul                                        |
| inverse + 乘                | `generic_prove.rs:305-316` | 见 G.2.4                                   |

> **FPGA**：**数据复制/广播 + LogUp GKR 逐层 sumcheck**占大头；`scatter-add` 型内存随机写（见下）需 bank 冲突处理。

#### G.3.2 Embedding / Token-index 散点组装（`embeddings.rs:366-465`）

> 证"输出=嵌入矩阵按索引取行"。输入是 `x[i]`(i64 token 索引)，用**稀疏 one-hot 散点累加**把索引归约成可域化向量，再对 `reduced_one_hot·嵌入矩阵` 做一次 sumcheck。

| 微算子                          | 源码位置                         | FPGA                                                                |
| ------------------------------- | -------------------------------- | ------------------------------------------------------------------- |
| 按行列拆段                      | `embeddings.rs:389`            | point 切分                                                          |
| EQ 位置 β 表                   | `embeddings.rs:397`            | EQ 展开(G.2.2)                                                      |
| **★ sparse scatter-add** | `reduced_one_hot[x[i]]+=β[i]` | **按索引写累加器（gather/scatter 内存，随机写需 bank 处理）** |
| 嵌入矩阵 pad/域化               | `embeddings.rs:414`            | 段A·域化                                                           |
| MLE 固定列变量(fold)            | `embeddings.rs:432`            | 段D 折叠同款                                                        |
| sumcheck(2 MLE 积)              | `embeddings.rs:443-444`        | 段C·G.1                                                            |

#### G.3.3 卷积 / FFT + 位置编码（`convolution/mod.rs:698+`，`positional/mod.rs:33-231`）

> 卷积走 **FFT 域批量 sumcheck**（`BatchFFTProof`）：`Y=iFFT(Σ_j FFT(X[j])∘FFT(W[j]))`——是把这个关系写成 GKR/多重 sumcheck，**不是 Fr 上的 NTT**。位置编码有两种 **variant 证明路径不同**：

| Variant            | 是什么                                      | 怎么证明                                                                                                                                 | 模型             |
| ------------------ | ------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | ---------------- |
| **Absolute** | 学习到的绝对位置 embedding（行=每位置向量） | 走 G.3.2 Embeddings 散点组装（position 索引固定 0..s，比 vocab 小）                                                                      | GPT-2            |
| **Rope**     | RoPE 旋转，cos/sin 预计算矩阵               | cos/sin 是模型级权重 MLE（`base_frequency_id` 提交）；对 Q/K 逐元素 `cos·x+sin·x'`，作系数作用参与 einsum/sumcheck，单独成节点证明 | Llama/Qwen/Gemma |

| 卷积微算子                        | 源码位置                                                   |
| --------------------------------- | ---------------------------------------------------------- |
| filter→FFT 张量重排              | `mod.rs:706`                                             |
| 挑战拆 (r1,r2)                    | `mod.rs:711-719`                                         |
| **RLC 加权散点累加 filter** | `mod.rs:736-744` `w1_reduced[k]+=β[i,j]·tensor[idx]` |
| φ-G GKR 折叠                     | `mod.rs:725`                                             |
| sumcheck(2 MLE 积)                | `mod.rs:756-762`                                         |

#### G.3.4 注意力掩码 / 相对位置（`attention_mask/prove.rs`）

| 微算子                                          | 源码位置           | 说明                                                              |
| ----------------------------------------------- | ------------------ | ----------------------------------------------------------------- |
| **固定行/列变量 MLE 归约（fix_mle_rec）** | `prove.rs:13-56` | 递归固定变量生成部分求值 EVAL 表（同 EQ，带 `update_acc` 累加） |
| EQ 多项式装箱                                   | `prove.rs:79-80` | 部分求值→EQ MLE，段D·D.1 用                                     |

> **s² 剪裁落点**（承附录 F §F.5）：注意力二次项（Softmax/CasualMask/attn-score）是长上下文主增长，G.3.4 的掩码部分求值正是"能否剪成认证行/下三角"的硬件判断位置（源码已含 `fix_lower_mle_rows/columns` 下三角 MLE，`attention_mask/prove.rs`）。

### G.4 Fiat-Shamir transcript —— Blake3（`arkyper/transcript/blake3.rs` + `transcript/mod.rs`）

| 微算子                   | 输入→输出                                                     | 硬件                                                               |
| ------------------------ | -------------------------------------------------------------- | ------------------------------------------------------------------ |
| 吸收 append_bytes        | 字节→blake3(state)                                            | blake3 压缩（1024-bit 块、7 round、state 512b）+ XOF               |
| **挤压 challenge** | `finalize_xof().fill(16B)` → `F::from_le_bytes_mod_order` | **128-bit LE→256b 归约 mod p（rejection-free，2^128 < p）** |
| 域→字节                 | Fr→32B 压缩; G1→(x,y) 各 32B + ∞ 哨兵                       | 打包                                                               |
| 挑战幂                   | q^i 逐次 mul                                                   | 串行 mul 链                                                        |

> **核心结论（硬件单元的收敛集）**：整条 prove = **一块 254-bit 蒙哥马利域单元（Fr/Fq 复用）+ G1 点运算（MSM/打开）+ Blake3 + 数据搬运（切分/复制/拼接/scatter）+ 逐节点/合并 sumcheck**。串行瓶颈（调度器要暴露流水）：sumcheck 轮间、折叠 ell 轮、witness Horner、挑战幂链。并行富矿：sumcheck 轮内 b、EQ 层内、fold 轮内 j、MSM 桶。

---

> 调度/带宽/瓶颈→[B](B_工作负载与瓶颈判定.md)；trace 存储量→[F](F_trace存储量核算.md)；FPGA 规格→[C](C_FPGA实现规格.md)；分段合理性/耗时占比→**本文附录 F**；微算子/密码单元/算子族下钻→**本文附录 G**。
