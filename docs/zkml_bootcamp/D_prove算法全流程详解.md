# D · Prove 算法全流程详解（入参 / 数据 / 操作 / 返回 / 数据结构）

> **定位（本文回答什么）**：把一次 zk prove 从**入口到序列化**的主干链路，按 **①~⑧** 分段逐段讲清**六件事**——**入参 / 使用的数据 / 操作 / 返回值 / 关键数据结构 / 操作了哪些数据**。核心对象是 `Prover::prove_chunk`（iop/prover.rs:542）：它承载段②~⑧的全部证明计算，其前的 `chunked_prove_local`→执行图→调度只是"切块+建图"的壳。**面向**：想搞懂软件到底怎么跑、准备对照源码或落硬件的读者。
>
> **分工（本文与相邻文档的关系）**：
> - **[H](H_prove全算子清单与数据流.md)** = 本文的**下一级分解**：把本文每一段再下拆成"可直接综合的微算子 + 数据流"，供 FPGA 落硬件。本文立"每步在软件里吃进什么、吐出什么"；读 H 拿"再往下的微算子清单"。
> - **[F](F_trace存储量核算.md) §7** = 本文附录 A 的**调用次数细账来源**（各模型 MSM/sumcheck 次数表）。
> - [A](A_关键事实与源码核定.md) / [B](B_工作负载与瓶颈判定.md) / [C](C_FPGA实现规格.md) = 证明主链之外的三篇：A 给硬事实与源码核定，B 给工作负载/瓶颈判定，C 给 FPGA 落地方案。本文只在需要时引用它们。
>
> **章节索引**：**§0** 先给"demo → prove_chunk"真实调用树 + ①~⑧ 竖直管线图 + 8 段数据流贯穿；**§①~⑧** 逐段展开六维；**附录 A~E** 是按角度查阅（A=调用次数/规模公式、B=算子清单、C=FPGA 阶段化、D=算子→源码对照、E=逆向阅读顺序）。
> **符号**：`F=Fr=ark_bn254::Fr`(32B)；`Element=i64`(8B)；`∇`(MLE)=`DensePolynomial<Fr>` 求值表 `2^n` 元素；`Trace=HashMap<NodeId,Step>`；`G1=BN254 曲线点`。**源码位置一律 `文件:行号`。**

---

## 0. 一次 prove 的总貌（先立地图，再看每一站）

> **总貌 = 一张真实调用树**：从 demo 按一次键 → 到 `prove_chunk`（真正的证明计算入口）。先把"prove_chunk 是被谁、在哪一行调出来的"钉死，再展开它内部的 ①~⑧。留给读者一句话：**所有证明计算都在 `prove_chunk` 里；它之前的一切只是"准备数据 + 建调度图"的壳。**
>
> **§0 分三层递进**：**0.1** demo → prove_chunk 的真实调用树（代码路径）；**0.2** ①~⑧ 竖直管线图（各段产出/消费）；**0.3** 8 段在 prove_chunk 的真实落点 + 3 根贯穿数据流。三者自上而下回答"从代码哪个入口进、每段做什么、彼此怎么联系"。

**0.1 demo 到 prove_chunk 的真实调用树**（代码逻辑，按行号可点）：

```
zkml/src/bin/bench/llm.rs:317   let (proof, io) = driver.prove(&prover_ctx, trace)?;
        │
        ▼
model/llm.rs:774  Driver<N>::prove                                  ←【应用层】模型专用壳
        ├─ :788 DefaultChunkingStrategy::from(&trace)               选切块策略（默认整模型 1 块）
        └─ :789 self.chunked_prove_local::<_,_,_,SequentialExecutor>(ctx, trace, Some(1), strategy, ())
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
        ├─ :973 GraphScheduler::new(exec_graph)                    建调度器
        └─ :974 Ex::run(&config, scheduler, inputs, context)      Ex=SequentialExecutor(scheduler.rs:161)
             │
             ▼
graph/scheduler.rs:161  SequentialExecutor::run                    ← 逐个取 ReadyNode 喂给 node.run
        └─ scheduler.rs:257  ReadyNode::run  →  self.node.run(ctx, inputs)
             │                                  node = Colored<ProverGraphNode, C>
             ▼
iop/prover_graph.rs:157  ProverGraphNode::run                       ←【真正 dispatch】match 三种图节点
        ├─ ProverSplit   (:159-187)  transcript.challenge_scalar(L171) → chunk.chunk_trace(L176)
        │                          → 产出 ChunkProveInput
        ├─ ChunkProver   (:190-207)  initialize_transcript_for_chunk(L200) → Prover::new(L201)
        │                          → chunk.chunk_layers(model)(L202)
        │                          → 【prover.prove_chunk(...)】(prover_graph.rs:203) ★
        └─ Final         (:209-221)  收集 ChunkProof → FinalProof(L221)
             │
             ▼  (★ 这一行进 prove_chunk，段②~⑧全部从这里进入)
iop/prover.rs:542  Prover::prove_chunk                             ← 真正的证明计算体
        ├─ :562  instantiate_witness_ctx_for_chunk   (段②域化+段③批量承诺)
        ├─ :583-666  output_claims_by_port + chunk_output_claims   (段④ claims 生成)
        ├─ :671-808  for (...) chunk.subgraph.backward_iter() → op.prove  (段⑤节点级 sumcheck)
        ├─ :827  prove_tables                          (⑤查表层 LogUp)
        ├─ :836  commit_prover.prove → claims_batching_sumcheck:140 → RLC → HyperKZG::open  (段⑥⑦)
        └─ :860-871  拼 ChunkProof 返回                    (段⑧序列化上游)
```

> **★ 从 demo 怎么快速对到 prove_chunk**：你按 `llm.rs:317 → driver.prove` 走，中途会看到 `Driver<N>::prove`（model/llm.rs:774）→ `chunked_prove_local`（prover.rs:995）→ `build_local_execution_graph`（prover.rs:891）→ `run_execution_graph`（prover.rs:962）→ `SequentialExecutor::run`（scheduler.rs:161）→ `ReadyNode::run`（scheduler.rs:257）→ `node.run`。**到 `node.run` 这里看起来"断了"——因为真正的 `prove_chunk` 不在 scheduler 层，它藏在下一步 `ProverGraphNode::run`（prover_graph.rs:157）的 `match` 里**：`ChunkProver` 分支那行 `prover.prove_chunk(...)` 在 **prover_graph.rs:203**。**再走一步就到了**——这一"夹心"就是第①步最容易被直觉漏掉、也最容易让"文档对不上代码"的根因。上面这张树把中间层全部补齐了。
>
> **正名（避免又"包一层"的混乱）**：仓库里有**两条**能到 prove_chunk 的壳，别混——
> - **`Driver<N>::prove`（model/llm.rs:774）**：LLM 应用专用壳，demo 走的就是它；它内部直接调 chunked_prove_local。
> - **`Prover::prove`（iop/prover.rs:1040）**：zkml 通用壳，又包一层 chunked_prove_local→SequentialExecutor；通常不直接从 bench 到这。
> 两条最后都在 `chunked_prove_local`（prover.rs:995）汇合 → 进 `ProverGraphNode::run` → `prove_chunk`。**文档段②~⑦ 讲的全是 `prove_chunk` 内部（prover.rs:542 起），与用哪条壳无关。**

**0.2 ①~⑧ 竖直管线：一次 prove 从入口到序列化的主干**

每一行读作「喂进什么 → 哪个算子 → 得到什么」。**域标**：`i64`=8B 域化前；`F`=Bn254Fr；`G1`=曲线点；`∇`=MLE 求值表。**①~⑧ 全部发生在 `prove_chunk`（prover.rs:542）这个函数体内**——0.1 的树只是把这个函数体"放到它该在的地方"。

```
① 入口/切块/调度
   full_trace(i64) + 权重 ──split_in_chunks→chunk_trace + 执行图(0=Split,C×ChunkProver,C+1=Final)
② 域化 + 算子化
   Element=i64 张量 ──to_field(i64→F) → pad 2^n → into_mle──▶ ∇ MLE
③ witness 批量承诺
   逐节点 ∇ ──切块(2^20) + batch_poly_msm(Pippenger)──▶ 每块 1 个 G1 承诺
④ claims 生成
   节点输出 ∇ ──evals(point)+EQ+挑战采样──▶ Claim{ point, value }
⑤ 节点级 sumcheck
   虚拟多项式(多∇×Challenge) ──逐轮 fold+build_uni_poly──▶ 轮多项式 + 输入 claims
⑥ 合并 sumcheck + RLC
   全 claims ──EQ+batching→单点；linear_combination(RLC)──▶ 1 条 rlc_poly
⑦ KZG 打开
   rlc_poly+单点 ──(i)折叠 2^m→常数 (ii)承诺折叠poly (iii) 3点 u=[r,−r,r²]──▶ OpeningProof
⑧ 序列化
   OpeningProof+各ChunkProof ──postcard+CanonicalSerialize──▶ Proof 字节流
```

**0.3 8 段在 `prove_chunk` 里的真实落点 + 3 根贯穿线**

代码里①②~⑧ **不是 8 次顺序调用**，而是被塞进 `prove_chunk`（prover.rs:542）里的**一次"承诺先行 + 一段逆拓扑循环 + 一个收尾合并"**。"8 段"是**数据在各抽象层的流动**，不是 8 份代码块。先给出**8 段各自落在 prove_chunk 哪一行的真实表**（prover.rs:542-871），再讲把它们拧成一股的 **3 根贯穿线**。

**8 段各自落在 `prove_chunk` 哪一行（prover.rs:542-871 真实调用顺序，含 ①② 为何不被计数的原因）**：

| 代码位置 | 做的事 | 对应文档段 |
|---|---|---|
| **:562** `instantiate_witness_ctx_for_chunk` | 逐节点生成 witness MLE + **一次性** `batch_commit` → `chunk_io_commitments` | **②域化+i64→Fr + ③witness批量承诺** |
| **:570-573** `ChallengeStorage::initialise` | 响应对应的 lookup 挑战 | ④的一部分（挑战存储） |
| **:583-621** `output_claims_by_port` | 对模型输出 tensor `to_field().pad_next_power_of_two()` → `compute_claim` 采样 | **④ claims 生成（"输出侧播种"）** |
| **:627-666** `chunk_output_claims` | 对外向边 tensor 同上 → transcript 采**共同求值点 `common_point`** → `mle.evaluate` 得出 eval → Claim | **④ claims 生成（跨 chunk 边界）** |
| **:671-808** `for (node_id, node) in chunk.subgraph.backward_iter()` → `op.prove` | **逆拓扑**逐节点跑 sumcheck，量出/回传 claims | **⑤节点级 sumcheck** |
| **:815-822** `compute_*_boundary_edges_claims` | 把边界边的 poly 挂进 witness claim | ⑤的辅助 |
| **:827** `prove_tables` | LogUp 查表证明 | ⑤的查表层 |
| **:836** `commit_prover.prove` → 内部 `claims_batching_sumcheck`(:140) → RLC → `HyperKZG::open` | **一个**合并 sumcheck + 线性组合 + 打开 | **⑥合并sumcheck+RLC + ⑦KZG打开** |
| **返回 `ChunkProof`** (**:860-871**) | Final 汇集+序列化 | **⑧序列化** |

> **读上表的关键**：③承诺在最前（:562）、④ claims 在中间（:583-666，分"输出 seed"+"边界 claim"两小块）、⑤ 一个逆拓扑 for 循环（:671）吃最小、⑥⑦ 在末尾一个 `commit_prover.prove`（:836）。**①②是最外面的两层壳，不产生证明计算**：①发生在 `prove_chunk` 之外（prover.rs:995 的切块/建图/调度，见 §0.1 树）；②的域化 i64→Fr 是 ③ 承诺与 ④ claim 采样前**逐节点即时发生**的取数动作（`section.output_tensors() → to_field().pad_next_power_of_two()`，见 :603-606、:704-708），随取随弃，不单独成段。**所以"8 段"的命名是个抽象好记的视角，真正的代码只分成 ③→④→⑤→⑥⑦ 四截 + 壳①。**

**把 8 段拧成一股的是下面 3 根贯穿线**：

1. **Claim 是唯一跨段传递的"货币"（⑤←④ 的因果链）**。④ 生成条目/输出侧 `Claim{求值点,求值}`（:583-621 播种模型输出、:627-666 播种外向边、`compute_claim`(:620)/`mle.evaluate`(:659)）；⑤ 的逆拓扑循环里，每个节点**先**用 `chunk.claims_for_node(node_id, &claims, &chunk_output_claims)`(:717) 吃下游已确立的 claim，**再** `op.prove` 求出本节点输入又生成新 claim，最后 `claims.extend(...)`(:792-797) 回传上游。**→ 一个节点的"输出 claim"成为更上游节点的"输入 claim"，claim 沿逆拓扑一条链传**：④ 播种输出端，⑤ 反着吃并续生出输入端 claim，直到闭合回模型输入端。这就是"为什么必须逆拓扑"（:178 注释的代码根因）。

2. **单个共享 transcript（Fiat-Shamir）把所有随机挑战串成时序**。从 `ProverSplit`(:L171 `challenge_scalar`) → ④的 `compute_claim`/`common_point` 采样 → ⑤每轮 sumcheck 的挑战 → ⑥RLC 挑战 → ⑦KZG 挑战，**全部从同一个 `self.transcript` 依次读**。这让 verifier 能按同样顺序重放、对每条 claim 与轮多项式严格校验。代码里靠 `&mut self.transcript` 一路传、没有显式"关联变量"，所以读起来"看不出"。

3. **MLE(∇) 是各段共同的"数据底座"（②③④⑤⑥⑦ 都操作同一批 ∇）**。② 把 i64 张量 `into_mle` 成 ∇ 求值表并记录；③ 全图 ∇ 一次 `batch_commit` 承诺（此后 ∇ 有了可验证的承诺值）；④ 在 ∇ 上采点算 claim（`into_mle`(:652)→`mle.evaluate`(:659)）；⑤ 在 ∇ 上逐轮 sumcheck（fold/uni-poly）；⑥ 把所有 ∇ 按挑战 `linear_combination` 成 1 条 rlc_poly；⑦ 打开 rlc_poly。**物理上同一批 ∇ 历经 5 道变换**：`into_mle → evaluate → fold → linear_combination → kzg_open_batch`，三者共享同一张 MLE。

> **一句话合拢三根线**：**④ 先定"证什么"（从 transcript 采样的输出/边界求值断言=claim）；⑤ 逆拓扑回溯，用 `op.prove` 把"输出 claim"逐节点归约成"输入 claim"回传上游，形成 claim 链；②③ 早已把全图 witness MLE 承诺好，⑤ 的每条 claim 最终都落到一条已承诺的 MLE 上；⑥⑦ 把残留在 claim 链末端的多项式按 transcript 挑战批量线性组合、一次合并 sumcheck 归约到单点、再 KZG 打开；①⑧ 只是外包的"切块调度"与"打包"。**

**两条根本规律**（贯穿全篇，先记住）：

- **MSM 次数 ∝ 域化元素总量 ΣQ**（随 trace 的 `s+s²` 涨），不由节点数决定——批量承诺是"一次调用吃下全部 witness，内部按 2^20 切块"，每块一个 Pippenger MSM。
- **sumcheck 实例数 ∝ 图节点拓扑（可证明节点数），不随 s 涨**；但**每个实例的规模 ∝ 该节点张量 2^n**（score 的 s² 尤其），所以 sumcheck"总代价"仍随 s 涨。

**执行位置速查**（FPGA 对照用，细到算子）：

| 步 | 算子                                                                | 需要的数据                               | 归属                               |
| -- | ------------------------------------------------------------------- | ---------------------------------------- | ---------------------------------- |
| ① | split_in_chunks → build_chunks → build_execution_graph            | full_trace + 权重                        | **主机喂**开头（只喂一次表） |
| ② | to_field / pad / into_mle                                           | 逐中间量                                 | 内部（纯数据变形）                 |
| ③ | gen_lookup_witness → batch_commit = batch_poly_msm                 | witness ∇；**MSM 基点=g1_powers** | 内部 +**SRS 预置**           |
| ④ | evals(point) + EQ + 挑战采样                                        | 节点输出 ∇                              | 内部 + 挑战内建 transcript         |
| ⑤ | op.prove（fix_axes→虚拟多项式→逐轮 sumcheck；查表层走 LogUp GKR） | 节点 ∇+claims                           | 内部 + 挑战内建                    |
| ⑥ | claims_batching_sumcheck + linear_combination                       | 全部 claims；EQ 的预置承诺               | 内部 + 挑战内建 + 依赖预置承诺     |
| ⑦ | HyperKZG::open（折叠→承诺→3点）                                   | rlc_poly+单点；**g1_powers**       | 内部 +**SRS 预置**           |
| ⑧ | CanonicalSerialize + postcard                                       | 各段 Proof 字段                          | 内部；**Proof 交主机**       |

> **★ FPGA 只做 ①~⑧ 能否完整跑通一次 prove？——能且正好闭环**（核实自 `iop/prover.rs:995/542/837`、`poly_commit/context.rs:149-210`、dp `arkyper/mod.rs:329-383`）：
>
> - **内部自足**：i64→Fr / pad / into_mle、MSM 切块(2^20/128 是代码常量)、witness 生成、逐节点 sumcheck、RLC、折叠、kzg_open_batch、序列化、以及全链 ④⑤⑥⑦ 的 **Blake3 挑战采样（最易漏的内建件**——挑战若由主机出，①~⑧ 就不自足、会逐轮爆炸）。
> - **必须 setup 预置（随 s 生成，提前装载）**：`SRS/g1_powers`（尺寸 `1<<max_num_vars`，**与 s 绑定**，`context.rs:166`）、`model_commitments`（含表承诺，`context.rs:210`）。③⑦ MSM 基点、⑥ EQ 都靠它们。
> - **主机只在首末**：喂 `full_trace+权重`，收 `Proof`（⑧）。
> - **求逆在主链非必需**：各 provable 层的 shift 用乘 `lt_eval` 处理（`generic_prove.rs:244-254`），全程无 `inv()`；仅 BatchFFT 旁路用两处 inverse（`prover.rs:345/474`）。

---

## ① 入口 / 切块 / 调度（壳层 · prove_chunk 之外）

**做什么 / 在哪**：把"整份 trace + 模型图"切成若干 chunk、建执行图、做调度，把证明分派到 `prove_chunk`。**本段只做数据切分/组装/调度，不做任何域运算**（H 段0）——是 `prove_chunk` 躲在外面的一层壳。

> **完整入口链已在 §0.1 给出**（demo → prove_chunk 全树，见上文）。本①节只讲被 §0.1 按下不表的**切块/建图/调度**细节。记住一句话：**①是壳，prove_chunk 才是计算体**——
> `llm.rs:317 → Driver::prove(model/llm.rs:774) → chunked_prove_local(prover.rs:995) → build_local_execution_graph(prover.rs:891) → run_execution_graph(prover.rs:962) → SequentialExecutor::run(scheduler.rs:161) → ReadyNode::run(scheduler.rs:257) → node.run → ProverGraphNode::run(prover_graph.rs:157) → ChunkProver 分支 → prover.prove_chunk(prover_graph.rs:203)`。**此后到段⑧全在 prove_chunk 内（prover.rs:542）。**

- **入口层**（本段①）只做「切块 + 组装执行图 + 调度」，**不做任何 sumcheck/承诺**。
- **per-chunk 层**（段落⑤~⑦）才是真正的证明计算：对每个 chunk 逐节点跑 `prove_chunk`。
- **衔接**：执行图 `ProverSplit` 把 full_trace 切成 chunk_trace，`ChunkProver` 再调 `prove_chunk`。
- **★ 为什么 `prove_chunk` 在 scheduler 层"找不到"**：`SequentialExecutor`/`ReadyNode` 只负责调度信号，`ReadyNode::run`(scheduler.rs:257) 里的 `node.run` 落在 **`ProverGraphNode::run`(prover_graph.rs:157)** 那个 `match` 上；`ChunkProver` 分支那行 `prover.prove_chunk(...)` 在 **prover_graph.rs:203**。所以从 `llm.rs` 直觉追到 `ReadyNode::run` 就"断了"——**再走一步（ProverGraphNode::run 的 match）就到 prove_chunk**（详 §0.1 树 ✓）。

**六维**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **入参**           | 根输入`full_trace: Trace<Element>`（整份前向中间量，i64）+ `Model<Element>`（模型图 + 权重），外加可选的 `num_chunks` 与切块策略                                                                                                                                                                                                                                                                                                                                    |
| **使用的数据**     | `ModelCtx` 的整张图、`Step` 表、各节点拓扑                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **操作**           | `split_in_chunks`(iop/prover.rs:1019) 选切块策略 → `ModelChunk::build_chunks`(chunking.rs:267) 按拓扑切模型图成子图+边界边+替换节点 → `replace_splitted_nodes`(prover.rs:1021) 在 trace 原位标记被切节点 → `to_verifier_io`(prover.rs:1023) 域化公共 IO → `build_local_execution_graph`(prover.rs:891/918-936) 建执行图 `ProverGraph` → `run_execution_graph`(prover.rs:962) 交给 `GraphScheduler`+`SequentialExecutor`(scheduler.rs:161) 做**调度**（只发信号、取 `ReadyNode`、调 `ReadyNode::run`(scheduler.rs:257)→`node.run`，**不着证明计算**），真正 proof 计算落在 `ProverGraphNode::run`(prover_graph.rs:157) 里：`ProverSplit`切 `chunk_trace`(chunking.rs:553) + 取 `chunk_layers`(chunking.rs:579) / `ChunkProver`触达 `prove_chunk`(prover_graph.rs:203) / `Final`合并 |
| **返回值**         | `Vec<ModelChunk>`（子图+外向边+replaced_nodes）+ `SplittedNodesInfo` + 执行图 `Graph`（`0=Split, 1..=C=ChunkProver, C+1=Final`）+（最终）`Proof`                                                                                                                                                                                                                                                                                                                |
| **关键数据结构**   | `ModelChunk`（子图 + 边界边 + 替换节点）；执行图 `ProverGraph`；`chunk_trace: Trace<Element>`（只含本块节点 Step）；`ChunkingStrategy`                                                                                                                                                                                                                                                                                                                            |
| **操作了哪些数据** | **只切表、不碰数值**。full_trace 按 chunk 子集选出 → 每块仍是 `HashMap<NodeId,Step>`，Step 里是 `Vec<TensorHandle>`(i64 句柄)。**数据搬运/子集选取，零域运算**                                                                                                                                                                                                                                                                                           |

**入口每个函数的入参/出参/做什么**：

| 调用（位置）                                 | 入参→出参                                             | 做什么                                                                             |
| -------------------------------------------- | ------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| `Prover::prove`(prover.rs:1040)            | `ctx`+`full_trace`+`model` → `(Proof, IO)`    | 选`DefaultChunkingStrategy::from(&full_trace)`、`Some(1)` 整模型 1 chunk，交棒 |
| `ctx.split_in_chunks`(prover.rs:1019)      | 上下文+策略 →`(Vec<ModelChunk>, SplittedNodesInfo)` | 委托`ModelChunk::build_chunks`(chunking.rs:267) 切子图、算边界边、replaced_nodes |
| `full_trace.replace_splitted_nodes`(:1021) | trace + split_info → 就地改                           | 横向切时把被切节点的 step 替换成多个子 step                                        |
| `to_verifier_io`(:1023)                    | trace →`IO<F>`                                      | 把模型输入/输出`pad_next_power_of_two().to_field()`（verifier 核对对象）         |
| `build_local_execution_graph`(:1029/891)   | chunks →`ProverGraph`                               | node0=ProverSplit、中间=ChunkProver、末尾=Final                                    |
| `run_execution_graph`(:1032/962)           | 执行图+输入+上下文 →`Proof`                         | 交给 `GraphScheduler` + `SequentialExecutor::run`(scheduler.rs:161)：调度器 `next_ready_nodes` 推/拉 `ReadyNode`，`ReadyNode::run`(:257) 调 `node.run` → `ProverGraphNode::run`(prover_graph.rs:157) 分派到 Split/ChunkProver/Final |

**多 chunk → Final 合并**（补齐主链最后一环）：执行图有 `C+2` 个节点：

```
ProverSplit ──chunk_trace_i──▶ ChunkProver(1..C)  每块各自 prove_chunk(②~⑦) 产出一个 ChunkProof
      各 ChunkProof ──────────▶ Final  合并成单个完整 Proof
```

- `ProverGraphNode::run`(prover_graph.rs:157) 是**调度真正落地的地方**——一个 `match self` 按图节点类型分派三路：
  - **`ProverSplit`**(prover_graph.rs:159-187)：`transcript.challenge_scalar()`(L171) 挤第一个共享挑战 → `chunk.chunk_trace(&full_trace)`(L176) 切 chunk_trace → 产出 `ChunkProveInput`(L181-185)。
  - **`ChunkProver`**(prover_graph.rs:190-207)：`initialize_transcript_for_chunk(input.challenge)`(L200) → `Prover::new(ctx, &mut transcript)`(L201) → `chunk.chunk_layers(ctx.model)`(L202) → **`prover.prove_chunk(...)`(prover_graph.rs:203)** → 产出 `ChunkProof`(L207)。**★ 从此处（prover_graph.rs:203）才进段②~⑦**。
  - **`Final`**(prover_graph.rs:209-221)：收集所有 `ChunkProof` → `FinalProof(outputs.into())`(L221)。
- `ProverSplit.run`：初始化第一个共享挑战 `challenge` → 切 chunk_trace → 逐 chunk 初始化 transcript（Fiat-Shamir 连续）。**每块自此是"同一次证明的独立并行支路"**——FPGA 块间铺并行的地方。
- `ChunkProver.run`：`initialize_transcript_for_chunk(input.challenge)` → `Prover::new(ctx, &mut transcript)` → `chunk_layers = chunk.chunk_layers(model)` → `prover.prove_chunk(...)` → 产出 `ChunkProof`。
- `Final.run`：把 C 个 ChunkProof 合并成一个完整 Proof。
- 默认 `num_chunks=None/Some(1)` 时只有 1 块，ProverSplit 不切、Final 直接收；`C>1` 才走并行聚合链。**对 FPGA**：块数 C = 端到端内核的并行度上限。

> **你对不上的真正断点在这**（把上面读进代码阅读顺序）——
> 你从 `llm.rs:317` 一路 `chunked_prove_local`→`run_execution_graph`→`Ex::run`→`SequentialExecutor::run`→`ReadyNode::run` 到了 `scheduler.rs:257` 的 `self.node.run(ctx, self.inputs.drain(..).collect())`。**这句 `node.run` 里 `node` 是 `Colored<ProverGraphNode, C>`，它的 `.run` 走 `ExecNode for ProverGraphNode` → `ProverGraphNode::run`(prover_graph.rs:157)。** 到这一句你其实已经很接近了——再进去一层就是 `prover_graph.rs` 那个 `match`，`ChunkProver` 分支的 `prover.prove_chunk(...)` 就在 **prover_graph.rs:203**。文档 D 段②~⑦ 的全部内容（sumcheck/承诺/RLC/KZG）正是从这里进去的。**所以不是文档带你去错地方，是你停在 `scheduler.rs` 那层、没再多走一步到 prover_graph.rs。**

> **概念落地**（图·节点·端口·子图·执行图·拓扑）：
>
> - **模型图** = 一个 `Node::Inner(L)` 一个模型算子，描述整个网络，一次建好。
> - **子图** = `ModelChunk.subgraph`（`ChunkedGraph`），从大图按层割出的拼图；incoming/outgoing 是"源/目标在别的 chunk 的边"，让 chunk 独立可证仍能对上外沿 claim。
> - **执行图** = 描述"一次 prove 的调度步骤"的另一张图（node0 Split、ChunkProver、Final）。**模型图告诉你"网络长什么样"；执行图告诉你"证明怎么分派几步跑"**。
> - **端口** `Step.node_inputs/node_outputs` 按端口排序，prove 靠端口号对齐上下游张量取数。
> - **逆拓扑** `backward_iter`：从输出往输入，每个节点在后继访问完才出现——必须逆序，因证明一层 = 在其输出张量上立 claim、再把 claim 当输入 claim 回传前驱；**链必须从模型输出端逆着闭合回输入端**（"必须等推理全完成才可 prove"的代码根因）。

---

## ② 域化与算子化：i64 → Fr → MLE（数据变形）

**做什么 / 在哪**：把 trace 里存的 i64 中间量转成 Fr、补到 2 的幂、再重解释成多线性扩展（MLE 求值表）。这是"i64 数值流通证 → Fr/∇"的**唯一一次域化**，之后全程在 F / G1 上流转。**也是 prove 里最贴 trace 的一次大规模搬运+转换**（主循环 `output_tensors().to_field().pad_next_power_of_two()`）。

**六维（逐节点固定三步）**：

| 六维                     | 内容                                                                                                                                                                                                                                                  |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **入参**           | `chunk_trace.get_step(node_id)` 取出的该节点 `Step`；其中 `section.output_tensors()` 是该节点所有输出张量（i64）                                                                                                                                |
| **使用的数据**     | 该节点的输入/输出 张量、`Shape`（是否 2 幂）                                                                                                                                                                                                        |
| **操作**           | `(a) to_field()`：`i64→F` 逐元素（`quantization/mod.rs:263-267`，负 i64 归约 mod p）；`(b) pad_next_power_of_two()`：尾部补 0 到 2^k（`tensor/mod.rs:812-836`）；`(c) into_mle()`：行主序重解释为超立方体求值表（`tensor/mod.rs:400`） |
| **返回值**         | `Vec<Tensor<F>>`（2^k 元素域张量），可 `into_mle()` 得 ∇                                                                                                                                                                                         |
| **关键数据结构**   | `Step`（node_inputs/node_outputs/proving_data）、`Tensor<Fr>`、`DensePolynomial<Fr>`(∇)                                                                                                                                                        |
| **操作了哪些数据** | trace 的逐 i64 张量 → Fr（8B→32B，形状不变）→ 补 0（记录 unpadded_shape）→ 重解释为 ∇。**就地 repack，大多是数据搬运不是算术**                                                                                                             |

**MLE 是什么存储、什么序**：一个张量 `[s,768]` 域化后补到 `[s',768]` 使总元素数 = `2^k`，`into_mle()` 把它逐元素平铺成一个 `DensePolynomial`：长 `2^k` 的数组，下标 = 布尔超立方体 `{0,1}^k` 的行主序编码。下标 `idx = b₀+2·b₁+…+2^{k-1}·b_{k-1}` 对应求值 `ℳ(b₀,…,b_{k-1})`；`num_vars=k`。**这是求值形式**（每项 = 一个输入张量的整数转域）。理解 sumcheck"折叠"的关键：把数组当超立方体的表。

> **两处补 pad 的口径**：layer 与 trace 都会 pad（`generate_contexts` 里 input_shape 先 `next_power_of_two`，主循环节点输出再 pad 一次），可后续优化只 pad 一次。
>
> **FPGA**：i64→Fr 最省 `|i64|` 常数模归约 + 符号位补 p；pad=搬移+补 0；into_mle=无拷贝重解释（连续内存则零运算）。

---

## ③ witness 批量承诺（MSM · 重量级）

**做什么 / 在哪**：把每个走 lookup 的节点的 witness MLE（查表的输入/输出域化结果）+ 查表多重度 + chunk 边界多项式，**一次性**批量承诺。**每 block 最重的一步（≈57% 耗时）**，源码落点 `generate_lookup_witness_for_chunk` → `batch_commit`。

**六维**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                         |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **入口链**         | `instantiate_witness_ctx_for_chunk`(iop/prover.rs:1114) → `generate_lookup_witness_for_chunk`(lookup/context.rs:224) → 全 chunk 一次 `batch_commit`(lookup/context.rs:420 / poly_commit/context.rs:325)                                                                                                                                              |
| **入参**           | 本块`chunk`、`chunk_trace`、可用 `lookup_ctx`（整库 GELU/softmax/norm/logits 表）、`chunk_layers`                                                                                                                                                                                                                                                    |
| **使用的数据**     | 本块节点（顺拓扑`forward_inners`）；每个走查表的节点从 trace 取输入/输出张量；多重度计算                                                                                                                                                                                                                                                                   |
| **操作**           | ① 并行逐节点`gen_lookup_witness`(lookup/context.rs:256)：对 GELU/softmax/norm/logits 从 step 产 witness MLE（如 GELU 的 input_mle+output_mle）+ `element_counts`；② `multiplicities`(lookup/context.rs:313)：`mult_i = 被查次数 ÷ 表出现次数`（LogUp 输入）；③ `all_polys` 拼一个 Vec **一次性** `batch_commit`(lookup/context.rs:420) |
| **返回值**         | `chunk_commitments: Vec<CommittedPolynomial>`（每个 witness 多项式的承诺集，每 2^20 块一个 G1 点）                                                                                                                                                                                                                                                         |
| **关键数据结构**   | `ChunkedCommitment`（`chunk_commitments: Vec<PCS::Commitment>` + 原 ∇）；`LookupWitness{logup_witnesses, table_witnesses, chunk_commitments}`                                                                                                                                                                                                         |
| **操作了哪些数据** | 各节点 witness ∇ + 全部 multiplicity + chunk 边界多项式 → 切块 → 每块一个 G1 承诺                                                                                                                                                                                                                                                                         |

**批量切块与 MSM**（poly_commit/context.rs:58-115）：

```
num_chunks_per_poly = ceil(2^poly_vars / 2^20).min(128)   # >2^20 才切，上限 128
每块 = 一次 size-2^20 Pippenger MSM: C = Σ aᵢ·(g^{τ^ᵢ})  (arkyper/mod.rs:471 batch_poly_msm)
MSM 块数总 = Σ_polys ceil(2^poly_vars/2^20) = ΣQ/2^20    # ΣQ = trace 域化元素总量
```

> **★ 分清两类承诺（别混）**：
>
> | 承诺类型                                  | 何时                                        | 内容                                                                                                                                         | 源码                               |
> | ----------------------------------------- | ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------- |
> | **模型/表承诺（setup 期，一次性）** | `generate_contexts` 构建 ProverContext 时 | 各层固有权重 MLE（model_polys）+ lookup 查询表`committed_columns`（如 GELU 表输出列），走一次 `batch_commit`，表用独占 `table_node_id` | `poly_commit/context.rs:170-214` |
> | **witness 承诺（每 chunk）**        | prove_chunk 每块开头                        | 逐节点输入/中间输出 MLE + lookup multiplicity                                                                                                | `lookup/context.rs:224`          |
>
> 模型/表承诺**不进 chunk、不随 s 变**，全局共享；段③ `batch_commit` 只覆盖 witness 层。**FPGA**：表承诺是少量固定 G1 点，可预置，不计入 per-prove MSM。

> **为何 MSM 次数少、sumcheck 次数多**：MSM 是"批量化、块化"的（一次调用吞全部 witness），次数由**域化元素总量÷2^20** 定；而 sumcheck 是"逐节点颗粒"的。节点多只让 `batch_commit` 打包更多多项式，不增加调用数。

**关键数据结构全景**（贯通各段，设计意图一句话：证明的输入是"图 + Trace 表"，不是逐 token 状态）：

| 结构                     | 定义点                  | 内容                                                                                 | 设计意图                                                                             |
| ------------------------ | ----------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| `Trace<Element>`       | `model/trace.rs:29`   | `steps: HashMap<NodeId,Step>` + 输入/输出句柄 + 可选 split 信息                    | 一次证明的**全部输入**；`Element=i64`(8B)                                    |
| `Step<Element>`        | `model/trace.rs:388`  | `node_inputs: Vec<TensorHandle>` + `node_outputs: NodeOut`(outputs+proving_data) | 每个图节点的**端口级**快照；proving_data 存 LN mean/std、softmax shift、argmax |
| `ModelChunk`           | `iop/chunking.rs:145` | 子图`ChunkedGraph` + incoming/outgoing 边 + replaced_nodes                         | 把大模型拆成可分布式/局部 prove 的块                                                 |
| `ProverContext<F,PCS>` | `iop/context.rs:36`   | PCS 参数 + 承诺上下文 + lookup 上下文                                                | **Setup 产物**，持全部已承诺权重及其承诺（静态，每次 prove 复用）              |
| `ChunkedCommitment`    | `poly_commit/mod.rs`  | 承诺多项式的块化视图 + 逐块求值/承诺                                                 | >2^20 元素 MLE 切成 2^20 块，按块做 Pippenger MSM                                    |
| 执行图`Graph`          | `iop/prover.rs:906`   | Split + C×ChunkProver + Final                                                       | 把 prove 调用序列建模成可调度图，支持多 chunk 并行                                   |

---

## ④ claims 生成：输出 claim + 边界 claim（播种）

**做什么 / 在哪**：为逆拓扑 sumcheck 播种种子。证明是先算**输出侧 claim** + 本块**外向边 claim**，逆序才走得下去——断言"MLE 在某个点上的求值"。prove_chunk 主循环前（prover.rs:583-621 + 627-666）。

**六维**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **入参**           | 本块`chunk` 结构、`chunk_trace`、transcript                                                                                                                                                                                                                                                                                                                                                  |
| **使用的数据**     | ① 本块"模型输出边"（`chunk.model_outputs_in_chunk()`）；② 本块"外向边"（`chunk.outgoing_edges`）；两者对应的节点输出 tensor（i64）                                                                                                                                                                                                                                                         |
| **操作**           | ①`output_claims_by_port`：取输出边 source 节点 step → `output_tensor_at` → `to_field().pad_next_power_of_two()` → `compute_claim(transcript, tensor)` 在该输出的 MLE 上采一点算 claim；② `chunk_output_claims`：取外向边源 tensor → `into_mle()` → **transcript 采共同求值点 `common_point`**（一点点增长，保证 Fiat-Shamir）→ `mle.evaluate(eval_point)` → Claim |
| **返回值**         | `output_claims_by_port: HashMap<ChunkedOutput, Claim>`（每个模型输出端口 → 求和 claim）+ `chunk_output_claims: HashMap<NodeOutput, Claim>`（本块输出、别的块要用的 claim）                                                                                                                                                                                                                  |
| **关键数据结构**   | `Claim{ eval_point: Vec<F>, eval: F }`；`compute_claim` 里 `evals(point)`+EQ 构造（`poly/eq.rs`）                                                                                                                                                                                                                                                                                        |
| **操作了哪些数据** | 输出/外向边张量 → i64→Fr→pad → 在 MLE 上求值 → 打包成 Claim。`output_claims` 喂给主循环 `Node::Output` 节点**播种**；`chunk_output_claims` 与 `claims` 合并后供跨 chunk 消费                                                                                                                                                                                                  |

> **claim 是"逆序的货币"**：一个节点既是一层输出又是下一层输入。sumcheck 在节点 A 输出上给"B 需用的某点求值 claim"，这 claim 回传给 A 当**输入 claim**；逆序保证后层先有 claim、前层拿到继续证自己的输入。**链从模型输出端逆着闭合回输入端**——这就是"必须等推理全完成才可 prove"的代码根因。
>
> **末 token 分支**（H 段4.0 / F §3）：LLM 只证明最后一行 logits，`final_proj→argmax` 的 argmax 值分开参与证明，其余 `s-1` 行 logits **不进 sumcheck、不承诺**。vocab(50257) 宽那一次 logits 是常数规模的单行粒度，不随 s 放大。

---

## ⑤ 节点级 sumcheck：`op.prove`（逆拓扑主循环）

**做什么 / 在哪**：逆拓扑逐节点证明"节点的输出 = 输入做该运算"。**sumcheck 主耗时的落点**（实例数≈可证明节点数），源码落点 `backward_iter` 主循环里的 `op.prove`。

**六维（主循环，prover.rs:671）**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **入口链**         | `for (node_id, node) in chunk.subgraph.backward_iter()`（逆拓扑）→ 每节点 `op.prove`（einsum/prove.rs:45 等）→ `IOPProverState::prove`(dp sumcheck/prover.rs:116)                                                                                                                                                                                                                                                                                                                                                            |
| **入参**           | `chunk.subgraph`、`chunk_trace`、`chunk` 的 claims 寄存器、`ctx`（LayerCtx）                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **使用的数据**     | 该节点`step`（trace 里的输入/输出张量 i64）、下游传来的 claim、`op` 层实现                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| **操作**           | ①`get_step` 取节点 step；② `output_tensors().to_field().pad_next_power_of_two()` 域化该节点输出；③ `claims_for_node` 收集下游给本节点的 claim；④ `flatten_and_merge_claims`(= `merge_claims`，same_poly::Prover，prover.rs:1103) 把指向同一多项式的多 claim 合成单点；⑤ `op.is_provable()` 才 `op.prove`（立 sumcheck 证 "输出=输入运算"，并把结果转成本节点**输入**端口 claim 回传）；非可证明层（split/recombination）只透传；⑥ `claims.extend(... NodeInput::new(node_id,i))` 存输入 claim 供更上游取 |
| **返回值**         | 本节点**输入**端口的 `Vec<Claim>`（追加进全局 `claims`）；主循环末尾 + 边界 claim 补登 + LogUp 表证明 + 收口                                                                                                                                                                                                                                                                                                                                                                                                               |
| **关键数据结构**   | `claims: HashMap<NodeInput, Claim>`（`NodeInput=(node_id,input_port_index)`，逆拓扑每处理一节点就为它每个输入端口存一个 claim）；`Node::Input/Inner/Output` 三类                                                                                                                                                                                                                                                                                                                                                               |
| **操作了哪些数据** | 该节点输出 i64→Fr→pad→∇ → 在其上立 sumcheck → 产出输入 claims 写回`claims` 寄存器。逐节点，逆序                                                                                                                                                                                                                                                                                                                                                                                                                              |

**逐函数入参/出参/做什么**：

| 调用                                                        | 入参                      | 出参                            | 做什么                                       |
| ----------------------------------------------------------- | ------------------------- | ------------------------------- | -------------------------------------------- |
| `chunk_trace.get_step(node_id)`                           | 本块 trace                | `&Step`（输入/输出张量句柄）  | 取该节点中间量                               |
| `.to_field().pad_next_power_of_two()`                     | i64 输出                  | `Vec<Tensor<F>>`(2^k)         | **i64→Fr+补 2 幂**（段②逐节点落点）  |
| `claims_for_node(node_id, &claims, &chunk_output_claims)` | 已算 claims + 外向 claim  | `Vec<Claim>`                  | 收集"本节点输出端口"被哪些下游引用           |
| `flatten_and_merge_claims(...)`                           | 下游 claim + 输出 tensor  | `Vec<Claim>`                  | 同输出端口多 claim → 合成"该多项式单点取值" |
| `op.is_provable()`/`op.prove(...)`                      | 层实现+ctx+claims+section | 本节点**输入**端口 claims | 可证明层立 sumcheck 证关系；非可证明层透传   |
| `claims.extend(...NodeInput::new)`                        | 本节点输入 claim          | 更新`claims`                  | 按输入端口号存 claim，上游逆序后取           |

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
> **claim 寄存器语义**：`NodeOutput` 播种的 claim 不在 `claims` 里，来自段④ `output_claims_by_port`；遇到输出节点先把 `(node_id,0)` 种子塞进 claims，让上游取到"我输出应在的值"。

---

## ⑥ 合并 sumcheck + RLC：`commit_prover.prove`（收口）

**做什么 / 在哪**：把本 chunk **所有已生成 claim**（node sumcheck 产物 + model 权重 claim + 表 claim）**归约到单点**，再 RLC 塌缩成**一条**多项式。**每 chunk 1 次**。这是"只一次打开"的根因。

**六维**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **入口链**         | prove_chunk 收尾`commit_prover.prove`(poly_commit/prover.rs:97) → `claims_batching_sumcheck`(prover.rs:140) → `build_sumcheck_expression`(poly_commit/mod.rs:93) → 1 次合并 sumcheck → `linear_combination`(RLC) → `PCS::prove`(prover.rs:125，落到段⑦)                                                                                                                                                                                                                                        |
| **入参**           | `witness_commitments`（本块全部被承诺多项式块）、`ctx.commitment_ctx`、transcript                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **使用的数据**     | 全部 committed polys（含 model/表承诺）+ 每个的有效 claim + EQ 多项式                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **操作**           | ① 对每个 claim：`evals(poly.eq_point_per_chunk(point)).into_mle()` 构造 EQ 多项式（"只在点 p 处=1"的指示函数）+ `batching_coefficients_for_chunks`(`C_j = eq(j, point)`)；② `build_sumcheck_expression`：每 (多项式,claim) 组装成 `挑战·(Σ_j C_j·P_j_chunk)·eq_p`，全项累加成**一个**虚拟多项式；③ 一次 `IOPProverState::prove` 归约到单点；④ `transcript` 对每个 committed poly 采随机挑战 `c`；⑤ `linear_combination`(dense.rs:934)：`rlc = Σ c_i·P_i` → 1 条 rlc_poly |
| **返回值**         | `ClaimsBatchingProof{ sumcheck_proof, sumcheck_evals, sumcheck_point, committed_polys }` + `rlc_poly`；随后 `PCS::prove` 打开（段⑦）                                                                                                                                                                                                                                                                                                                                                                    |
| **关键数据结构**   | EQ 多项式、batching 系数、`VirtualPolynomial`、`rlc_poly: DensePolynomial`                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **操作了哪些数据** | 全部 committed polys + 全 claims → 合并 sumcheck 归单点 → RLC 塌缩成 1 条 → 交给打开。**term 数 = Σ(每 poly 的 claims × chunks)** = "多 claim 一次归约到单点"的规模口径                                                                                                                                                                                                                                                                                                                             |

> **合并 sumcheck 的公式含义**：`eq_p(x)=∏_i(pᵢxᵢ+(1−pᵢ)(1−xᵢ))`——把"在点 p 求值 v"这个标量 claim 表达成超立方体求和；batching 系数 `C_j=eq(j, p 高位)` 把切块拼回完整点（`P(r)=Σ_j eq(j,高位)·P_j(低位)`）。
>
> **RLC → 打开**：对每个 committed poly 采随机挑战 c，`rlc=ΣcᵢPᵢ` 把"很多多项式在单点"塌缩成"**一个**多项式在单点"；随后 PCS::prove 只打开这一个 rlc。**"只一次打开"就来自 RLC 在前**。

---

## ⑦ KZG 打开：`HyperKZG::open`（打开）

**做什么 / 在哪**：对 RLC 后的那条 `rlc_poly`，在单点 `open`，产出防伪的打开证明。**每 chunk 1 次**。**≈3n MSM + 折叠**，是打开主耗时。

**六维**：

| 六维                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **入口链**         | `PCS::prove`(arkyper/mod.rs:497，先求值) → `HyperKZG::open`(:329) → `kzg_open_batch`(:186)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| **入参**           | `rlc_poly` + 单点 `point` + prover_params（SRS `g1_powers`）+ transcript                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| **使用的数据**     | rlc_poly 的 ∇ 求值表、挑战点、`pk.g1_powers()`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **操作**           | **Phase1 折叠**：`polys[i+1] = rᵢ·(p[2j+1]−p[2j]) + p[2j]`，逐变量把 `2^m` 压到常数（每轮减半，`ell−1` 轮），轮间串行/轮内并行（arkyper/mod.rs:349-356）。**Phase2**：`msm::batch_poly_msm(pk.g1_powers, &polys[1..])` 承诺 `ell−1` 个折叠中间多项式 → transcript 采 `r` → 构造 `u=[r,−r,r²]`（3 个特殊打开点，:373）。**Phase3 `kzg_open_batch`**(:186)：`v[i][j]=f_i(u_j)=eval_as_univariate` → `q_powers = challenge_scalar_powers` → 对 `ell` 个折叠多项式**再 RLC** 按 `q_powers` 组合成 `b_poly`（:216）→ `kzg_batch_open_no_rem(&b_poly, u)`(:222) 对 B 在 3 点打开 |
| **返回值**         | `OpeningProof`：`w=(w₀,w₁,w₂)` 三个 G1 witness + `v`（各折叠在 3 点的求值）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| **关键数据结构**   | `polys` 折叠链、`coms`(ell−1 个 G1)、`u=[r,−r,r²]`、`b_poly`、witness polynomials                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **操作了哪些数据** | rlc_poly → 折叠（把超立方体的边按 rᵢ 加权合并，体积每轮减半）→ 承诺折叠中间 → 由`q_powers` RLC 出 B → 3 点批量 open。**打开总 MSM ≈ Phase2 的 ell−1 折叠 + Phase3 的 3 点 open（≈承诺 ×4）**                                                                                                                                                                                                                                                                                                                                                                                                                              |

> **为何 3 点 u=[r,−r,r²]**：HyperKZG 展开三点求值让验证者用**一个 pairing 等式**验 3 个求值（由 `d₀=d₁²` 二次关系联动），比逐点开省 N−1 次 pairing。
>
> **为何 open 里又 linear_combination**：把 ell 个折叠**再**按挑战线组合成 B，一次 3 点打开省掉重复。
>
> **打开是新的一次 RLC + MSM**，不是直接 open 大表：折叠把 rlc_poly 压成常数个多项式，再做 3 点 open。**串行依赖链**（第 i 轮折叠依赖第 i−1 轮）对流水不友好。

---

## ⑧ 序列化 / 证明打包（出证）

**做什么 / 在哪**：把 OpeningProof 与各 ChunkProof（含 TableProof）打成一个完整 `Proof` 字节流交给 verifier。**纯本地**。

**六维**：

| 六维                     | 内容                                                                                                                                       |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| **入口链**         | `Final.run` → `run_execution_graph` 汇集各 ChunkProof → `iop/mod.rs` Proof 结构 → postcard 序列化（提交 187d6c9b）+ ark_serialize |
| **入参**           | 各段产出的 Proof 字段：`ChunkProof`(iop/mod.rs:67)、`TableProof`(:138)、`OpeningProof`(poly_commit/mod.rs:155)、output_evals 等      |
| **使用的数据**     | 域元素（F）、曲线点（G1）、轮多项式、commitment                                                                                            |
| **操作**           | `CanonicalSerialize` 压缩：Fr→32B LE、G1→(x,y) 各 32B（∞ 哨兵）；postcard 把各段 proof 结构扁平化/拼接成连续字节                      |
| **返回值**         | `Proof`（二进制字节流）──▶ verifier                                                                                                   |
| **关键数据结构**   | `Proof` / `ChunkProof` / `TableProof` / `OpeningProof`                                                                             |
| **操作了哪些数据** | 段①~⑦ 所有产物 → 打包成证明文件；主机在末尾收回                                                                                         |

> **FPGA**：域→字节打包（bus 编解码）+ proof 结构顺序拼装（长度前缀）；无算术。

---

## 附录 A · 调用次数与规模公式（FPGA 调度口径）

供"每步该拉几次引擎、每次吃多少"。设 `C`=chunk 数（默认 1），`V`=可证明节点数，`ΣQ`=域化元素总量。

| 段②~⑦                       | 调用次数                      | 随什么变         | 规模                                                      |
| ----------------------------- | ----------------------------- | ---------------- | --------------------------------------------------------- |
| batch_commit（witness，段③） | `1 次/chunk = C`            | 不变             | 内部按 2^20 切`≈ΣQ/2^20` 个 Pippenger MSM（∝ s+s²） |
| model 承诺（段③ setup）      | `1 次/整模型`（Setup 静态） | 不变             | 权重元素数/2^20 个 MSM                                    |
| 节点级 sumcheck（段⑤）       | `≈ V 次/chunk`             | **不随 s** | 每实例 num_vars 轮，每轮 O(2^余×多项式×次数)            |
| 合并 sumcheck（段⑥）         | `1 次/chunk = C`            | 不变             | 变量数=chunk 最大 num_vars；term 数=Σ(claims×chunks)    |
| PCS 打开（段⑦）              | `1 次/chunk = C`            | 不变             | ≈ 3n MSM + 折叠（随 ΣQ）                                |

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

| #  | 算子                                                           | 归属段 | 输入 → 输出                         | 数据搬运                         | 若留主机 → 交互                   |
| -- | -------------------------------------------------------------- | ------ | ------------------------------------ | -------------------------------- | ---------------------------------- |
| 1  | **i64→Fr 域化** `to_field`/`pad`                    | ②     | trace(i64 8B) → Fr MLE(32B)         | trace 大批量写(∝ΣQ)            | 整份 trace 要搬，仍可流式          |
| 2  | **张量→MLE** `into_mle`                               | ②     | Fr 张量 → 超立方体表                | 就地/搬入                        | 需先域化后生成                     |
| 3  | **★ EQ 求值** `evals(point)`                          | ④⑥   | 点 → EQ 2^k 表                      | 中量                             | 每 claim 生成一次                  |
| 4  | **★ 批量承诺** `batch_commit`                         | ③     | Σ 个 2^20-chunk MLE → 承诺         | **大批量流式(32MB/chunk)** | **主计算**                   |
| 5  | **★ 节点级 sumcheck** `IOPProverState::prove`         | ⑤     | 虚拟多项式 → sumcheck proof+claims  | 逐轮                             | **主计算 + 逐轮**            |
| 6  | **★ 合并 sumcheck**（含 `build_sumcheck_expression`） | ⑥     | 全部 witness+model+EQ → 单点        | 逐轮                             | 主计算 + 逐轮                      |
| 7  | **RLC 线性组合** `linear_combination`                  | ⑥⑦   | Σ committed polys × 挑战 → 1 poly | 读全部已承诺块                   | 面对大 witness 也要搬一遍          |
| 8  | **折叠 fold**（open Phase1）                             | ⑦     | 多项式 → k-1 轮折叠                 | 逐轮                             | 逐轮                               |
| 9  | **★ 打开 MSM**（≈3n）`PCS::prove`                    | ⑦     | 折叠后 → 打开证明                   | **大批量 MSM**             | 主计算                             |
| 10 | **LogUp/GKR**（仅 lookup 节点）                          | ⑤     | lookup 表+multiplicity → GKR        | 中等                             | 多一块引擎                         |
| 11 | **挑战采样/transcript**`(Blake3)`                      | 全程   | 消息 → 随机挑战                     | 小量但高频                       | **最关键：逐轮，须贴近引擎** |
| 12 | **状态编排**（claims map、proof 组装）                   | 全程   | flags                                | 少量                             | 可留主机但需 chunk 级同步          |

> **重计算 = 3 类**（批量 MSM、sumcheck、EQ/fold）；**必配配套 5 类不可缺**（域化、转 MLE、EQ 生成、RLC、挑战采样）。FPGA 要做一个**端到端 prove 内核**——主机只喂 trace+权重，板内完成 1→12，只在首尾/边界交互。**仅做 MSM+sumcheck 而把配套留主机，会退化成逐轮爆炸。**

---

## 附录 C · FPGA 阶段化加速方案

| 阶段化                                         | 覆盖 prove 段           | 加速比潜力         | 难度           | 说明                                               |
| ---------------------------------------------- | ----------------------- | ------------------ | -------------- | -------------------------------------------------- |
| **M0：仅 MSM 引擎（①②④的 MSM 部分）** | witness 批量承诺 + 打开 | 承诺+打开 ~60–90% | ★★★ 中      | 一个 Pippenger 引擎，块=2^20；需 HBM 喂 32MB/chunk |
| **M1 = M0 + sumcheck 引擎**              | ①②③④ 全流程         | sumcheck +35%      | ★★★★ 中-高 | 要一块 sumcheck 多轮引擎(EQ/fold)，块级 handler    |
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

1. 先 `<F5>` `iop/prover.rs:1040` → `:995`，看**一切从整份 trace 进去**（段①）。
2. 主干进 `:542 prove_chunk`，盯 **`:671 backward_iter` 主循环**（段②④⑤ 的骨架）。
3. 每节点跳 `layers/einsum/prove.rs:45`，体会"一次 sumcheck 证明一个 GEMM"（段⑤）。
4. 看 `lookup/context.rs:420` 一行，明白 witness 是 **per chunk 一次批量承诺**（段③）。
5. 看 `poly_commit/prover.rs:97/140`，懂"所有 claim → 一个合并 sumcheck → RLC → 一次打开"（段⑥⑦）。
6. 最后到 `arkyper/mod.rs:329`（折叠）+ `:186`（3 点 open）看**最底层叶片**（段⑦）。

---

> 调度/带宽/瓶颈→[B](B_工作负载与瓶颈判定.md)；trace 存储量→[F](F_trace存储量核算.md)；FPGA 规格→[C](C_FPGA实现规格.md)；算子级微拆与硬件单元→[H](H_prove全算子清单与数据流.md)。
