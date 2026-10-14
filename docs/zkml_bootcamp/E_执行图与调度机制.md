# E · 执行图构建与调度机制（GraphScheduler / SequentialExecutor / ProverGraphNode）

> **本文回答什么**：zkml 一次 chunk 化证明，在执行图这一层的完整运行机制。覆盖三大件——**执行图怎么建**（`build_execution_graph_internal`）、**executor 怎么跑**（`SequentialExecutor::run`）、**scheduler 怎么调度**（`GraphScheduler` 的 `init_nodes` / `mark_done` / `next_ready_nodes` / `is_done`）——并把 **`ProverGraphNode::run` 的三类节点（ProverSplit / ChunkProver / Final）** 放进这个循环里，讲清它们彼此的调用与迭代关系。
>
> **面向**：准备对照 [prover.rs](zkml/src/iop/prover.rs)、[executor.rs](zkml/src/graph/executor.rs)、[scheduler.rs](zkml/src/graph/scheduler.rs)、[prover_graph.rs](zkml/src/iop/prover_graph.rs) 读源码，或想彻底搞懂"一次 prove 在调度层到底怎么流转"的读者。
>
> **与相邻文档的关系**：本文是 [D](D_prove算法全流程详解.md) §0.1 那张调用树的**调度层细解**（D 只给树、不细讲循环）；[H](H_prove全算子清单与数据流.md) 讲 prove_chunk 内部的微算子（调度层之下的内容）。本文只覆盖"执行图 = 3 类节点 + 调度循环"，**不进入 prove_chunk 内部**。
>
> **先立框架**：要理解这段代码，必须先分清三个**井水不犯河水**的抽象层次——它们各有各的"节点"概念，别混：
>
> | 层次 | 文件 | "节点"指什么 | 谁用 |
> |---|---|---|---|
> | **① 模型网络图** | `model/mod.rs` | `Layer`（一层神经网络） | `prove_chunk` 内部逐节点跑 sumcheck |
> | **② 执行图**（本文主场） | `prover.rs` 建、`scheduler.rs` 调度 | `ChunkProver` 等三大类节点 | 调度器 + executor |
> | **③ 微算子** | `H_prove…` | 域化/MLE/承诺/打开算子 | 落到硬件时使用 |
>
> 本文从进 ② 这一层：`execution_graph` 的节点**不是 model 的 layer**，而是 `ProverSplit / ChunkProver / Final`。**gpt2 和 llama2-7B 的 layer 数差异永远到不了这一层**——它们被整个装进一个 `ChunkProver`，调度层看不见。

---

## 目录

1. [总览：一次 chunk 化证明的调度三角色](#1-总览一次-chunk-化证明的调度三角色)
2. [角色A —— 执行图是什么：`build_execution_graph_internal`](#角色a--执行图是什么build_execution_graph_internal)
3. [角色B —— 调度器 GraphScheduler：就绪判定与数据搬运](#角色b--调度器-graphscheduler就绪判定与数据搬运)
   - [B.1 数据结构与状态机](#b1-数据结构与状态机)
   - [B.2 `init_nodes`：填入第一批](#b2-init_nodes填入第一批)
   - [B.3 `mark_done`：产出写边、收集图输出](#b3-mark_done产出写边收集图输出)
   - [B.4 `next_ready_nodes`：挑出下一批](#b4-next_ready_nodes挑出下一批)
   - [B.5 `is_done`：全量完成判定](#b5-is_done全量完成判定)
4. [角色C —— executor 的 while 循环：`SequentialExecutor::run`](#角色c--executor-的-while-循环sequentialexecutorrun)
5. [节点本体 `ProverGraphNode`：三类节点的 run](#节点本体-provergraphnode三类节点的-run)
   - [5.1 `ProverSplit`（入口 / 切块）](#51-proversplit入口--切块)
   - [5.2 `ChunkProver`（主计算）](#52-chunkprover主计算)
   - [5.3 `Final`（收尾 / 打包）](#53-final收尾--打包)
6. [三者的拧合：一次完整运行的时序与迭代关系](#6-三者的拧合一次完整运行的时序与迭代关系)
7. [设计意图总结](#7-设计意图总结)

---

## 1. 总览：一次 chunk 化证明的调度三角色

一次 chunk 化证明（`chunked_prove_local`，prover.rs:995）在调度层由**三个角色**协作完成：

```
      ① 建图（prover.rs）                      ② 跑图（executor.rs）               ③调度（scheduler.rs）
分  chunked_prove_local ──build_local_execution_graph┐─►  SequentialExecutor::run
工    ──graph_inputs────────────────────────────►        │
关                                    SecuentialExecutor::run
键    产出一张【执行图】                          对这张图
                                                    while !is_done() {
                                                      对 ready_nodes 逐个 run
                                                      mark_done 写边
                                                      next_ready_nodes 取下一批
   应用程序入口(ran)                                                       了
```

分工的一句话：
- **角色 A（建图，prover.rs）**把"切好的 chunks"包装成一张**有向无环执行图**，图节点是 `ProverSplit / ChunkProver / Final`，边表达依赖。
- **角色 C（调度器，scheduler.rs）**给这张图配上**状态机**（谁在跑、谁完成、边上有无数据），只回答两个问题：**"现在有哪些节点就绪？"** 和 **"全部完成了吗？"**。它不执行计算，只做"数据搬运 + 就绪判定"。
- **角色 B（executor，executor.rs）**是**总控循环**：问调度器要一批就绪节点，逐个执行，把结果交回调调度器写边，再要下一批，直到 `is_done`。

> **核心心法（全文反复出现、务必先记住）**：**"数据流动"依赖执行图的边，每边带一个 `weight: Option<IO>` 槽；"就绪判定"依赖这个槽是否被填上。** 一个节点只有当它的**所有入边 weight 都已填上数据**（= 所有前驱都产出了）才会被调度器放行进 ready。这就是整层代码的魂。

我用一张执行图的结构 + 运行轮次图贯穿全文（C=3 块示例，节点数 = `1 + C + 1 = 5`）：

```
                       ┌───────────────┐
                       │  ProverSplit   │  ← 唯一 source 节点（入口、切块、播种挑战）
                       │     色 0       │     持有 chunks: Vec<ModelChunk>（个数恒定）
                       └───────┬───────┘
        ┌──────────────────────┼──────────────────────┬─────────────────────┐
        │ 边 (0,0)             │ 边 (1,0)             │ 边 (2,0)            │  weight 初始 None
        ▼                      ▼                      ▼                     ▼
   ┌───────────────┐     ┌───────────────┐     ┌───────────────┐
   │ChunkProver(0)│     │ChunkProver(1)│     │ChunkProver(2)│  ← C 个 ChunkProver，各自独立证明一块
   │     色1      │     │     色2      │     │     色3      │
   └───────┬──────┘     └───────┬──────┘     └───────┬──────┘
           │ 边 (0,0)           │ 边 (0,1)            │ 边 (0,2)
           ▼                    ▼                     ▼
                       ┌───────────────┐
                       │     Final     │  ← 唯一 sink，收集全部 C 个证明
                       └───────────────┘
```

对应运行轮次（C=3，深度 = 最长依赖链 2 条边 → **3 轮**）：

| 轮 | 就绪节点（ready_nodes） | 每个节点 run | mark_done | 产生 |
|---|---|---|---|---|
| 1 | `[ProverSplit]` | 切块 → `Vec<ChunkProveInput>`×3 | 写满 3 条 split→chunk 边 | 3 个 chunk 就绪 |
| 2 | `[ChunkProver(0),(1),(2)]` | 各 prove_chunk → ChunkProof | 写满 3 条 chunk→final 边 | Final 就绪 |
| 3 | `[Final]` | 收集 3 证 → FinalProof | 产出图输出 | 全图 done |

> ⚠️ **轮数 vs 节点数 vs chunk 数的关系**（易混淆，先钉死）：**轮数 = 图依赖深度 + 1**（=3），**不随 chunk 数变**；chunk 数改变的是**第 2 轮批内节点数**（C=10 时第 2 轮一口气跑 10 个，仍是 3 轮）；节点执行**总次数 = 节点总数**（每个节点恰好 run 一次）。三者互不相等、独立变化。

下面按 建图 → 调度 → executor → 节点 run 逐级展开。

---

## 角色A —— 执行图是什么：`build_execution_graph_internal`

**源码**：prover.rs:906-938。它是两层壳 `build_local_execution_graph`（prover.rs:891）→ `build_execution_graph_internal(chunks, local=true)` 的内核。

### A.1 函数意图

输入：`chunks: Vec<ModelChunk>`（切块的成品，个数就是被证明的块数）和一个 `local` 标志。
输出：一张 `ProverGraph`——`ExecGraph<ProverGraphNode, usize>`，即"以 ProverGraphNode 为节点、usize（颜色）为着色、边默认无权重"的执行图。

它把"**逻辑上要做的事**（切块 + 每块证明 + 收尾打包）"转成一张**物理可调度的 DAG**，让调度器能靠边的依赖判断顺序。

### A.2 逐行拆解

```rust
let mut exec_graph = ProverGraph::new();          // :916 空图
let num_chunks = chunks.len();                     // :917 记录块数（=C）

let init_node_id = exec_graph.add_inner(           // :918-927 加"入口节点"
    ProverGraphNode::ProverSplit(if local {        //   :919-925
        SplitNode::new(chunks).disable_dry_trace() //    local=true 时避免 drying trace
    } else { SplitNode::new(chunks) }).colored(0), //    :926 着色 0
)?;

let final_node_id = exec_graph.add_inner(          // :928 加"收尾节点"
    ProverGraphNode::Final.colored(0))?;

for i in 0..num_chunks {                           // :930-936 每块一个证明节点
    let color = i + 1;                             //   :931 着色 1..=C
    let node_id = exec_graph.add_inner(
        ProverGraphNode::ChunkProver(i).colored(color))?;  // :932
    exec_graph.add_edge(init_node_id, node_id, (i, 0), None)?; // :934 split→chunk_i
    exec_graph.add_edge(node_id, final_node_id, (0, i), None)?; // :935 chunk_i→final
}
Ok(exec_graph)                                     // :937
```

- `add_inner`：加一个 `Inner` 节点，返回 `NodeId`。
- `add_edge(from, to, (src_port, dst_port), weight=None)`：建一条有向边。**第三参是端口对 `(源输出口, 目标输入口)`**，第四参是边的初始 weight。这里传 `None` = **初始无数据** → 目标节点初始"未就绪"。
- 边的端口号有讲究：
  - `(i, 0)`：split 的第 i 个输出口 → chunk_i 的第 0 输入口。**i 用来区分给哪一块**。
  - `(0, i)`：chunk_i 的第 0 输出口 → final 的第 i 输入口。**i 用来区分收集的哪块证明**。
- 着色 `color`：执行图按颜色做调度策略（`ReleasePolicy::UniqueColoring`，见下），同色节点不并行。这里 split/final 色 0、每块各一色，保证按依赖串行。

### A.3 建图结论

- **节点总数为 C+2**（1 split + C chunk + 1 final），与模型网络层数**无关**。
- **唯一 source** = ProverSplit（无入边）；**唯一 sink** = Final（有 C 条入边）。
- C=1（demo/bench 默认）时图只有 **3 个节点**。
- 建图本身**不执行任何东西**，只是组装拓扑 + 边槽。

> **为什么建图时把模型层数整个藏进一个 ChunkProver？** 这是"调度粒度的选择"：执行图的最小调度单元是 **chunk**（可并行证明的独立块），模型内部的几百个 layer 不参与调度，它们在 `prove_chunk` 里串行处理。这样：调度层看不到模型规模（gpt2/llama2 都是同一张 3 节点图），模型规模的差异被隔离在 chunk 内部——好扩展（加并行 executor 只在 chunk 间并行）。

---

## 角色B —— 调度器 GraphScheduler：就绪判定与数据搬运

**源码**：scheduler.rs。它的职责是"给执行图配一个状态机"，让 executor 只问"下一批是谁 / 完了没有"，而不用关心依赖拓扑。

### B.1 数据结构与状态机

结构体（scheduler.rs:283-302）：
```rust
pub struct GraphScheduler<N: ExecNode, C> {
    graph: ExecGraph<N, C>,                    // 图（节点带载荷，边带 weight 槽）
    waiting_input_data: HashMap<NodeInput, N::IO>, // 挂起的、非 source 节点的外部输入
    running_nodes: HashSet<NodeId>,            // 正在跑的节点
    done_nodes: HashSet<NodeId>,               // 已完成的节点
    release_policy: ReleasePolicy,             // 放行策略（默认 UniqueColoring）
}
```

- **`ReleasePolicy`**（scheduler.rs:200-217 附近）：控制"就绪节点是否允许放行"。`UniqueColoring` = **同色节点不同时跑**；也可换 `ReleasePolicy::All`（ThreadPoolExecutor 用，最大化并行）。默认 `UniqueColoring`（scheduler.rs:319）。
- **两个集合 `running` / `done`**：`running_nodes` 管"在跑"，`done_nodes` 管"已完成"，两者配合 `is_done` 判断 + 防重跑。
- **`waiting_input_data`**：统一接口注入的、但 node 不是 source 的输入暂存（见上轮 `init_nodes` 讨论，zkml 星型图不用它）。

**ExecNode trait**（scheduler.rs:14）：执行图节点要能被 run，必须实现：
```rust
pub trait ExecNode {
    type IO;           // 节点流经的数据类型
    type Context;      // 共享执行上下文
    fn run(&self, ctx: &Self::Context, inputs: Vec<Self::IO>) -> Result<Vec<Self::IO>>;
}
```
`ProverGraphNode` 就是这个 trait 的一次实现（`IO = ProverGraphIO`，`Context = LocalProverCtx`）。

**状态机流转**：每个节点生命周期三步——`pending → running → done`，由调度器三方法驱动：
```
init_nodes / next_ready_nodes   mark_done 收编
   (放到 ready)     ─────────►   running       done
     节点就其绪                   在跑          已完成
```

### B.2 `init_nodes`：填入第一批

**源码**：scheduler.rs:365-409。只在最开头调用一次（executor.rs:106）。

```rust
pub fn init_nodes(&mut self, input_data: HashMap<NodeInput, N::IO>)
    -> Result<Vec<ReadyNode<N, C>>>
{
    // 前置校验：各集合必须干净 (370-374)
    let mut inputs: HashMap<NodeId, BTreeMap<PortId, N::IO>> = ...;
    for (node_input, io) in input_data {                 // :378
        if self.graph.is_source(node_input.node_id) {    // :379 若是 source
            // 按端口收进 inputs，为该 source 建 ReadyNode (380-386)
        } else {
            self.waiting_input_data.insert(node_input, io); // :388 否则挂起
        }
    }
    // 对每个有输入的 source，组 ReadyNode，标记 running (396-406)
    Ok(ready)
}
```

- 遍历**整个 input_data**（是 HashMap，可能多个元素；zkml 只有一个 source + 一份 trace，故此时只会走 `if` 分支，`else` 分支为通用性兜底——详见本文 [附录A](#附录a---为什么-input_data-只有1个-还要用-for排序))。
- 产出第一批 `ReadyNode`：**构成首批的就绪节点**。zkml 里 = `[ProverSplit]`（唯一 source）。
- 把 source 标记进 `running_nodes`（:399），准备外包执行。

### B.3 `mark_done`：产出写边、收集图输出

**源码**：scheduler.rs:429-467。executor 每跑完一个节点就调一次。

```rust
pub fn mark_done(&mut self, node_id: NodeId, outputs: &[N::IO])
    -> Result<BTreeMap<NodeOutput, N::IO>>
{
    self.running_nodes.remove(&node_id);     // :435 结束"在跑"
    self.done_nodes.insert(node_id);         // :439-441 标记"已完成"
    for (_, edge) in self.graph.outgoings_mut(node_id)
        .filter(not done/running)            // :444-451 只写给未完成的下游
    {
        let output_port = edge.ports()[0].source_port;
        edge.weight = Some(outputs[output_port].clone());  // :461 ⭐ 产出写进下游边
    }
    // 把不属于下游端口的剩余输出作为"图输出"返回 (442, 463-466)
}
```

**这方法是整层的数据管道核心**：一个节点跑完，它的输出被本方法**分发到它所有下游边的 `weight` 槽**。每填一条边 = 下游目标往"就绪"又近一步。

### B.4 `next_ready_nodes`：挑出下一批

**源码**：scheduler.rs:481-541。每轮结束后取下一批。

```rust
pub fn next_ready_nodes(&mut self) -> Result<Vec<ReadyNode<N, C>>> {
    // 统计每个目标节点，其入边 weight 是否都已填上 (483-498)
    let ready_node_ids = self.graph.edges()                    // 遍历所有边
        .fold(HashMap<NodeId, Vec<bool>>::new(), |acc, (_, e)| {
            acc.entry(e.target()).or_default().push(e.weight.is_some());  // 边上有无数据
        })
        .into_iter()
        .filter(|(_, edges)| edges.iter().all(|r| *r))   // ⭐ 所有入边都填上 = 就绪
        .filter(|(node_id, _)| !self.running_nodes.contains(node_id)) // 且没在跑
        .map(|(n, _)| n).collect();

    for node_id in ready_node_ids {                          // :500
        if !self.release_policy.accept(node_id, self) { continue; }  // :504 放行策略
        self.running_nodes.insert(node_id);                   // :507 标记在跑
        // 收齐该节点所有入边的 weight (509-524)
        let mut input_data = BTreeMap::new();
        for (_, edge) in self.graph.incomings_mut(node_id) {
            let data = edge.weight.take().unwrap();           // :520 取走边的数据
            for port in edge.ports() {
                input_data.insert(port.target_port, data.clone());  // 按端口收集
            }
        }
        // 合入挂起的外部输入 (528-533)
        ready.push(ReadyNode {
            node: self.graph[node_id].as_inner().unwrap().clone(),
            inputs: input_data.into_values().collect(),        // :536 组输入Vec
            node_id,
        });
    }
    Ok(ready)
}
```

**就绪判定（最关键的一步，:493-494）**：一个节点 = 就绪 ⟺ 它的**每条入边 weight 都已是 `Some`**（= 每个前驱都已 `mark_done` 且把数据放到了这条边上）。这就是依赖自动满足的机制——**不必手动 DFS，边上的 weight 是否填上就是依赖是否满足的天然标志**。

**输入收集（:509-533）**：为每个就绪节点，把它所有入边的 `weight.take()` 取走，按端口号拼成它的 `inputs` Vec——这就是传给 `ProverGraphNode::run(ctx, inputs)` 的那份参数。

### B.5 `is_done`：全量完成判定

**源码**：scheduler.rs:547-549。
```rust
pub fn is_done(&self) -> bool {
    self.running_nodes.is_empty() && self.done_nodes.len() == self.graph.node_count()
}
```
- **不看轮数、不看输入还有没有**，只看：没有在跑的节点 **且** 完成的节点数 = 全图节点总数。
- 因为每个节点 run 后必 `mark_done`（且 `done_nodes.insert` 防重，重复会 panic），所以当 `done_nodes.len() == node_count` 时，**图里每一个节点都至少完成过一次**——这就是"证明全部 node done"的数学保证（而非"轮够了"）。

---

## 角色C —— executor 的 while 循环：`SequentialExecutor::run`

**源码**：executor.rs:100-123。它是**总控**，把建图结果 + 调度器串成最终执行。

```rust
fn run(_config, mut scheduler, input_data, context) -> Result<BTreeMap<NodeOutput, N::IO>> {
    let mut ready_nodes = scheduler.init_nodes(input_data)?;  // :106 首批 = [ProverSplit]
    let mut outputs = BTreeMap::new();                        // :107 收集图输出
    while !scheduler.is_done() {                              // :108 ⭐ 直到全部done
        // step1: 本批每个节点各跑一次
        let node_outputs = ready_nodes                        // :109-112
            .iter_mut()
            .map(|node| node.run(context))                    //  限制 每个 node.run
            .collect::<Result<Vec<_>>>()?;                    //   → Vec<Vec<IO>>
        // step2: 逐个 mark_done，把产物写边、记图输出
        ready_nodes.drain(..).zip(node_outputs)               // :113-119
            .for_each(|(node, output)| {
                let graph_outputs = scheduler.mark_done(node.node_id, &output).unwrap();
                outputs.extend(graph_outputs);                //  记下属于图的输出
            });
        // step3: 取下一批就绪节点
        ready_nodes = scheduler.next_ready_nodes()?;          // :120
    }
    Ok(outputs)                                               // :122 返回图输出
}
```

**循环体三步**（对应注释 :92-96"rounds"）：
1. **跑完本批**：对 `ready_nodes` 里每个 `ReadyNode` 调 `run`（内部再调到 `ProverGraphNode::run`）。因为逐个 `.map`（不是并行），所以是**串行**，保证确定性。
2. **收尾本批**：`drain + zip + for_each`，每个节点一次回调，把它的输出 `mark_done` 写边 + 把图输出并入 `outputs`。
3. **取下批**：`next_ready_nodes()` 按依赖放行下一批，交给下一轮。

**三个量的变化逻辑（C=3）**：
- `ready_nodes`（批内节点数）：`1 → 3 → 1 → 0`（0 表示退出）。
- `node_outputs`（外层 = 批内节点数，内层 = 该节点产出数）：`[1→1] → [3→1] → [1→1]`（每块产出 1 份）。
- `for_each` 回调次数 = 批内节点数：`1 → 3 → 1`；总共 = 节点总数 = 5。

> **为什么示例里 node_outputs 是嵌套 `Vec<Vec<IO>>`？** 因为 `node.run` 一次可能产出多个 IO（如 ProverSplit 对 C 块产出 C 个 ChunkProveInput），`collect::<Vec<_>>` 在外层再包一层，于是外层=节点数、内层=每节点产出数。`zip` 用外层配对（一节点 ↔ 一元素），回调里拿到的 `output` 就是该节点的整个产出 Vec。

**`outputs`（BTreeMap<NodeOutput, IO>）**：只收集"图的全局输出"——即节点里那些**没被任何下游边消费的端口**（`mark_done` 返回的 `graph_outputs`，scheduler.rs:463-466）。星型图的图输出只来自 Final。最终 `run_execution_graph` 用 `.into_values().exactly_one()`（prover.rs:975-976）取出唯一那个 `Proof`。

---

## 5. 节点本体 `ProverGraphNode`：三类节点的 run

**源码**：prover_graph.rs。`ProverGraphNode` 是枚举（三种变体）+ `ExecNode` 实现。它的 `run(ctx, mut inputs)` 按 `self` 匹配该节点是哪一类，三类逻辑完全不同。

### 5.1 `ProverSplit`（入口 / 切块）

**变体持有**：`SplitNode { chunks: Vec<ModelChunk>, dry_trace: bool }`（prover_graph.rs 顶部）。`chunks` 在建图时被 `SplitNode::new(chunks)` 原样装入，**个数恒定 = num_chunks，全程只读**。

**run 逻辑**（prover_graph.rs，`ProverGraphNode::ProverSplit(split_node)` 分支）：
```rust
// inputs 必须是 1 个 ProverSplitInput(full_trace)（由 graph_inputs 提供）
let ProverGraphIO::ProverSplitInput(full_trace) = inputs.pop().unwrap() else { bail!(..) };
let mut transcript = Prover::initialise_transcript(ctx.ctx)?;   // 初始化整体 transcript
let challenge = transcript.challenge_scalar();                   // squeeze 公共挑战
split_node.chunks.iter()                                          // ⭐ 遍历持有一整份 chunks
    .map(|chunk| {
        let chunk_trace = chunk.chunk_trace(&full_trace)?;        // 从 full_trace 切出本块子 trace
        if split_node.dry_trace { chunk_trace.dry_handles(); }     // 分布式时干燥句柄
        Ok(ProverGraphIO::ChunkProveInput(ChunkProverInput {
            chunk: chunk.clone(), trace: chunk_trace, challenge,   // 每块一份输入
        }))
    })
    .collect()                                                     // → Vec<ChunkProveInput>（长度=C）
```

**职责**：吃整份 `full_trace` → 产出 C 个 `ChunkProveInput`（切块 + 干燥 + 附公共挑战）。它的 `Vec` 返回被 `mark_done` 按端口 i 写进三条 split→chunk 边，滋养 C 个下流。

### 5.2 `ChunkProver`（主计算）

**变体持有**：`usize`（chunk 序号）。

**run 逻辑**（prover_graph.rs，`ChunkProver` 分支）：
```rust
// inputs 是 1 个 ChunkProveInput（本块专属，从 split 那条边的 weight 取来）
let ProverGraphIO::ChunkProveInput(input) = inputs.pop().unwrap() else { bail!(..) };
let chunk_proof = measure(... || {
    let mut transcript = initialize_transcript_for_chunk(input.challenge);  // 用挑战初始化本块
    let prover = Prover::new(ctx.ctx, &mut transcript);
    let chunk_layers = input.chunk.chunk_layers(ctx.model);      // ⭐ 在这里才触到模型的层
    prover.prove_chunk(input.chunk.clone(), &input.trace, &chunk_layers)   // 主证明计算
})?;
Ok(vec![ProverGraphIO::ChunkProof(chunk_proof?)])                  // 产出 1 个 ChunkProof
```

**职责**：一个 `ChunkProver(i)` 只证明第 i 块——调 `prove_chunk` 跑完这块内部的全部 sumcheck/承诺/打开，产出 1 个 `ChunkProof`。**这是调度层"第一次也是唯一一次"接触到模型规模的地方**（`chunk_layers(ctx.model)` 拉出该块所有 layer），也是全流程最重的一步，但调度器只把它当一个黑盒任务。

### 5.3 `Final`（收尾 / 打包）

**变体持有**：无。

**run 逻辑**（prover_graph.rs，`Final` 分支）：
```rust
// inputs = 全部 C 个 ChunkProof（从 C 条 chunk→final 边 weight 汇总而来）
let outputs = inputs.into_iter()
    .map(|out| if let ChunkProof(p) = out { Ok(p) } else { bail!(..) })
    .collect::<Result<Vec<_>>>()?;
Ok(vec![ProverGraphIO::FinalProof(outputs.into())])                // 打包成唯一 Proof
```

**职责**：把收到的 C 个 `ChunkProof` 收拢成一个完整的 `Proof`。它是**纯收集器**（不新算），是图的唯一 sink，标着"全图要产出的那个最终证明"。

### 三类节点关系小结

| 节点 | 入参 inputs 来源 | 产出 IO | 写入的下游边 |
|---|---|---|---|
| `ProverSplit` | 图输入 `[ProverSplitInput]`（无前驱） | `Vec<ChunkProveInput>`（长度=C） | 3 条 split→chunk |
| `ChunkProver(i)` | 它那条 split→chunk 边的 `[ChunkProveInput]` | `[ChunkProof]` | 它那条 chunk→final 边 |
| `Final` | 全部 C 条 chunk→final 边汇总的 `[ChunkProof×C]` | `[FinalProof]` | 无（输出即图输出）|

**它们之间没有函数级跳转**——所谓"调用关系 / 迭代关系"，本质是 `mark_done` 写边 + `next_ready_nodes` 放行的**数据沿边流动**，见下一节时序。

---

## 6. 三者的拧合：一次完整运行的时序与迭代关系

把 建图(A) → 调度(B) → executor(C) → 节点(E) 串成**一次完整 run**（C=3）的逐轮时序。这是全文的"合页"，读完应能复述整层代码。

### 准备：`chunked_prove_local`（prover.rs:995-1033）
1. `ctx.split_in_chunks(num_chunks, strategy)` → `(chunks, split_info)`（:1019）
2. `full_trace.replace_splitted_nodes(...)`、取 verifier IO（:1021-1023）
3. `build_local_execution_graph(chunks)` → 执行图（:1029）= **角色A**
4. `graph_inputs(full_trace, &graph)` → `{ ProverSplit: ProverSplitInput }`（:1030）
5. `run_execution_graph::<SequentialExecutor>(...)`（:1032）→ 进入 **角色C** 循环

### `run_execution_graph`（prover.rs:962-990）
1. `GraphScheduler::new(exec_graph)`（:973）→ **角色B** 实例化
2. `Ex::run(&config, scheduler, inputs, context)`（:974）→ **角色C** 总循环
3. 取回的结果 `.exactly_one()` → 断言唯一的 `FinalProof`（:975-989）

### executor 主体（角色C 循环）逐轮

**初始化**（:106）`ready_nodes = init_nodes(inputs)` → `[ProverSplit]`，running={split}，done={}。

**轮1**（is_done: running非空 → 继续）
- map+run：`ProverSplit::run` 吃 `[ProverSplitInput]`，产 `Vec<ChunkProveInput>(3)`
- for_each(1次)：`mark_done(ProverSplit)` → 写 3 条 split→chunk 边的 weight = 各自 ChunkProveInput；done={split}
- next_ready：3 个 chunk 入边全填满 → ready=`[ChunkProver(0),(1),(2)]`，running={0,1,2}

**轮2**
- map+run(3次)：3 个 `ChunkProver` 各产 `[ChunkProof]`
- for_each(3次)：3 次 `mark_done(chunk_i)` → 写 3 条 chunk→final 边 weight；done={split,0,1,2}
- next_ready：Final 的 3 条入边全填满 → ready=`[Final]`

**轮3**
- map+run(1次)：`Final::run` 汇总 `[ChunkProof×3]` → `[FinalProof]`
- for_each(1次)：`mark_done(Final)` → FinalProduct 记为图输出进 `outputs`；done={split,0,1,2,final}
- next_ready：无新节点可放行

**终止**：`is_done()` → running 空 **且** done.len(5) == node_count(5) → true → 退出 while → `Ok(outputs)`。

### 归纳"迭代关系"
- **外层迭代**：executor 的 `while !is_done()`，轮次 = 依赖深度（最长链边数+1），本例 3 轮。
- **中层迭代**：每轮对 `ready_nodes` 的 `.map(run)`（跑批内节点）+ `.for_each(mark_done)`（批内节点收尾），次数 = 批内节点数。
- **内层迭代**：`next_ready_nodes` 内部的边遍历（判就绪）+ `incomings` 遍历（收集入参）。
- **节点间迭代（"谁叫谁"）**：不是代码递归，而是 **数据沿边 + 就绪判定** 的推拉：`mark_done(前驱)` 推数据进边 → `next_ready_nodes(目标)` 因边满而拉它进 ready → `目标.run`。整层没有一次"节点 A 直接 call 节点 B"的函数调用；唯一的入口是 executor 对 `ReadyNode::run` 的调用，而那里 `ready` 由调度器按批喂入。

---

## 7. 设计意图总结

把这段代码的优秀之处显式点出来，方便读者带着意图读代码：

1. **调度与执行解耦**：executor（Executor trait）与调度器（GraphScheduler）是两套独立抽象。换 executor（Sequential ↔ ThreadPool ↔ 未来 Distributed/GPU）不影响调度逻辑，只换"批内怎么跑"。这让 zkml 能随意插并行执行器。
2. **依赖拓扑天然因果**：把"依赖是否满足"编码成"边 weight 是否填上"，`next_ready_nodes` 一行 fold + filter 就完成就绪判定——**不需要显式 DFS/Petri 网**，且天然支持扇出（一个前驱喂多个目标）与汇总（多前驱喂一个目标）。
3. **执行图作为"调度粒度"的隔离层**：把模型网络（可能上千 layer）坍缩成 `ProverSplit/Final + N×ChunkProver` 的薄执行图。让调度关心的维度（可并行 chunk 数）与证明复杂度（layer 数 / 承诺量）分离。
4. **离线可序列化**：`ProverGraphNode` / `ProverGraphIO` 都 `Serialize/Deserialize`（prover_graph.rs），整张图与中间数据可被搬走——这是**分布式证明**（多机器分块）的基石，`SplitNode` 的 `dry_trace` 标志就是为此预留（prover_graph.rs `SplitNode::new` + `disable_dry_trace`）。
5. **确定性执行**：SequentialExecutor 用 `.map` 串行跑批、BTreeMap 存边/节点（graph.rs:428 `BTreeMap` 保证遍历序）、`done_nodes` 防重——全程顺序可复现，这是 ZK 正确性/可调试性对秩序的要求。
6. **所有权与性能处理**：数据沿边 `weight.take()` 移动而不是克隆（scheduler.rs:520），`inputs.pop().unwrap()` 取所有权（prover_graph.rs）——减少大 tensor 拷贝，契合证明重负载。

---

## 附录A —— 为什么 `input_data` 只有1个还要用 for 排序？

`init_nodes` 用 `for (node_input, io) in input_data`（scheduler.rs:378）。尽管 zkml 这一步只有 1 个元素，for 仍是对的写法，原因：
- **入参是 `HashMap`**（可能多 source 多输入），对集合遍历是通用正确姿势；写成 `.exactly_one()` 会把通用调度器钉死在"单输入图"上。反例：executor.rs 的测试图 `MathAST` 建了 3 个 Input 节点，`input_data` 有 3 项，for 需跑 3 圈给 3 个 source 各建一个 ReadyNode。
- **`else` 分支（非 source）**是给"内部节点既有前驱、又被外部喂输入"的通用图兜底：入场时唯一能看到全部 input_data，故必须趁此把非 source 输入挂进 `waiting_input_data`，等那个节点将来就绪再合入（scheduler.rs:528-533）。zkml 星型图只有一个 source，`else` 永不触发，但它支撑了整个通用基础设施的完整性。

---

## 附录B —— 快速定位表

| 你要找的 | 文件:行号 |
|---|---|
| 撞 执行图 | `build_execution_graph_internal` | prover.rs:906-938 |
| 撞 执行图（壳） | `build_local_execution_graph` | prover.rs:891-898 |
| 撞 唯一图输入 | `graph_inputs` | prover.rs:941-960 |
| 撞 executor 入口 | `run_execution_graph` | prover.rs:962-990 |
| 撞 主入口 | `chunked_prove_local` | prover.rs:995-1033 |
| 总循环 | `SequentialExecutor::run` | executor.rs:100-123 |
| 图结构 | `Graph` / `Node` / `Edge` | graph.rs:424 / 26 / 309 |
| 节点 trait | `ExecNode`（定义） | scheduler.rs:14 |
| 调度体 | `GraphScheduler` 结构 | scheduler.rs:283-302 |
| 首批 | `init_nodes` | scheduler.rs:365-409 |
| 写边+收图输出 | `mark_done` | scheduler.rs:429-467 |
| 下一批 | `next_ready_nodes` | scheduler.rs:481-541 |
| 完成判定 | `is_done` | scheduler.rs:547-549 |
| 三类节点 | `ProverGraphNode` | prover_graph.rs |
| 切块节点 run | `ProverSplit` 分支 | prover_graph.rs |
| 证明节点 run | `ChunkProver` 分支 | prover_graph.rs |
| 打包节点 run | `Final` 分支 | prover_graph.rs |
