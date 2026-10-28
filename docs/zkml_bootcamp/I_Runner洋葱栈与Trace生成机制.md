# Runner 洋葱栈与 Trace 生成机制详细设计

## 文档控制

| 项目 | 内容 |
| --- | --- |
| 文档编号 | I |
| 所属系列 | zkml_bootcamp |
| 版本 | 1.1 |
| 发布日期 | 2026-10-29 |
| 文档状态 | 已发布 |
| 适用代码基线 | `zkml/src/model/mod.rs`、`zkml/src/model/llm.rs`、`zkml/src/model/trace.rs`、`zkml/src/tensor/handle.rs`、`zkml/src/layers/transformer/mod.rs`、`zkml/src/layers/provable/mod.rs`、`zkml/src/graph/graph.rs`、`zkml/src/iop/prover.rs`、`zkml/src/iop/chunking.rs`、`zkml/src/iop/prover_graph.rs`（当前 master，行号以此为据） |

### 修订记录

| 版本 | 日期 | 修订说明 |
| --- | --- | --- |
| 1.0 | 2026-10-28 | 首次发布：Runner 洋葱栈组件设计、图执行模型、Trace 生成与 Token 生成流程 |
| 1.1 | 2026-10-29 | 补齐四块深入分析：`layer.evaluate` 内部与 `LayerOut` 三件套（§3.6）、Port/Feed 图连接与寻址机制（§3.7）、StoreRunner 发送数据逐项拆解（§3.3）；新增 §5 `prove_chunk`：Trace 消费与证明全流程（切分策略、执行图、逐阶段拆解、逐算子证明协议）；原 §5/§6 顺延为 §6/§7 |

## 目录

- [1 引言](#1-引言)
  - [1.1 目的](#11-目的)
  - [1.2 范围](#12-范围)
  - [1.3 读者对象](#13-读者对象)
  - [1.4 术语、定义与缩略语](#14-术语定义与缩略语)
  - [1.5 参考资料](#15-参考资料)
- [2 总体设计](#2-总体设计)
  - [2.1 架构概览](#21-架构概览)
  - [2.2 组件清单与职责划分](#22-组件清单与职责划分)
  - [2.3 Runner 栈的组装](#23-runner-栈的组装)
  - [2.4 节点调用的穿行模型](#24-节点调用的穿行模型)
- [3 组件详细设计](#3-组件详细设计)
  - [3.1 LayerRunner 接口契约](#31-layerrunner-接口契约)
  - [3.2 BaseRunner](#32-baserunner)
  - [3.3 StoreRunner](#33-storerunner)
  - [3.4 TraceRunner](#34-tracerunner)
  - [3.5 HandleLifetimeRunner](#35-handlelifetimerunner)
  - [3.6 LayerOut 与 layer.evaluate 内部机制](#36-layerout-与-layerevaluate-内部机制)
  - [3.7 Port 与 Feed：图连接与寻址机制](#37-port-与-feed图连接与寻址机制)
- [4 关键流程](#4-关键流程)
  - [4.1 模型图执行流程](#41-模型图执行流程)
  - [4.2 Trace 生成流程](#42-trace-生成流程)
  - [4.3 Token 生成流程](#43-token-生成流程)
  - [4.4 一致性保证](#44-一致性保证)
- [5 prove_chunk：Trace 的消费与证明全流程](#5-prove_chunktrace-的消费与证明全流程)
  - [5.1 从 Trace 到 chunk：切分策略与执行图](#51-从-trace-到-chunk切分策略与执行图)
  - [5.2 prove_chunk 逐阶段拆解](#52-prove_chunk-逐阶段拆解)
  - [5.3 逐算子证明协议](#53-逐算子证明协议)
  - [5.4 Trace 数据消费点汇总](#54-trace-数据消费点汇总)
  - [5.5 跨 chunk 一致性与 verifier 侧](#55-跨-chunk-一致性与-verifier-侧)
- [6 设计约束与不变式](#6-设计约束与不变式)
- [7 已知限制与权衡](#7-已知限制与权衡)
- [附录 A 代码索引](#附录-a-代码索引)

---

## 1 引言

### 1.1 目的

本文档规定 zkml 推理引擎中 Runner 栈（`HandleLifetimeRunner(TraceRunner(StoreRunner(BaseRunner)))`）的组件设计、职责划分与协作机制，并完整描述其驱动的三条核心流程：模型图执行、Trace 生成、LLM Token 生成。作为后续维护、性能分析与分布式扩展的权威设计参照。

### 1.2 范围

本文档覆盖 zkml 模型推理路径的调度与记账机制，以及由此产出的 Trace 的消费路径，包括：

- Runner 栈的组装顺序与组件契约；
- 单模型图内节点的拓扑执行与张量数据流；
- `Trace` 结构的产出过程及与 `GenStore` 的配合；
- LLM 场景下逐 token 推理循环与最终 Trace 生成（heavy run）；
- `layer.evaluate` 的内部机制与 `LayerOut` 三件套（v1.1 新增）；
- Port/Feed 图连接机制与 storage_key 寻址链路（v1.1 新增）；
- `prove_chunk` 对 Trace 的逐步消费：chunk 切分、witness 生成、claim 反向传播与逐算子证明协议（v1.1 新增，业务逻辑层面）。

不在本文档范围内：具体算子的数值实现细节、多项式承诺方案（PCS）的数学构造、store 的磁盘编码格式细节。证明协议只描述到"业务流程 + 消费了 Trace 的哪些数据"的粒度，数学细节见 H 系列文档。

### 1.3 读者对象

- zkml 引擎维护者与审阅者；
- 需要在 Runner 栈上扩展新横切能力（日志、采样、限流等）的二次开发者；
- 性能与内存分析工程师。

### 1.4 术语、定义与缩略语

| 术语 / 缩略语 | 定义 |
| --- | --- |
| Runner | 实现 `LayerRunner` trait 的类型，`run_layer` 为其核心钩子（mod.rs:118）；Runner 之间通过泛型 `inner` 装饰器嵌套 |
| 洋葱栈 | 本文对装饰器栈的称呼：每次节点调用沿 `HandleLifetimeRunner → TraceRunner → StoreRunner → BaseRunner → layer.evaluate()` 逐层进入、逐层返回 |
| handle | `TensorHandle<N>`；由 `storage_key`、store 引用、（可空的）内存张量副本三部分构成 |
| dry / wet | dry：删除 handle 的内存副本（handle.rs:451），数据仍可通过 store 的 `load()` 重载（handle.rs:375）；wet：副本当前驻留内存 |
| storage_key | 由 `NodeOutput` 格式化得到的字符串键（mod.rs:1109），是张量数据在 store 中寻址与跨层衔接的唯一标识 |
| proving data | 量化后的 `Element`（i64）张量，由 `ProvingHandle` 承载，供证明电路消费 |
| heavy run | `run_elements` 收尾处对完整句子的单次全图前向（llm.rs:566-573），是 Trace 的最终生成点 |
| KV 增量 | 层内 concatenation cache 机制：新输入仅 1 个 token 时先与缓存拼接再计算（transformer/mod.rs:19-117） |
| LayerOut | 层评估返回值三件套：`outputs`（输出张量）+ `proving_data`（证明辅助数据）+ `tracked_layer_data`（量化校准数据）（provable/mod.rs:207-214） |
| ProvingData / ProvingHandle | 层评估期 / 运行期两态的证明辅助数据枚举；`ProvingHandle` 将底层数据包装为可落盘的 handle（provable/mod.rs:44-71） |
| TrackedDataId | 量化校准通道中中间张量的字符串标识（provable/mod.rs:204） |
| Port / NodeOutput / NodeInput | 端点编址三元组体系：`NodeOutput(NodeId, port)` 编址某节点的第 port 个输出，`NodeInput(NodeId, port)` 编址输入侧（graph.rs:213-235） |
| Feed | 一条展平后的数据连接 `{source: NodeOutput, target: NodeInput}`，由边上的端口连接展开而来（graph.rs:518-556） |
| chunk（证明分块） | `ModelChunk`：模型的连续节点子图 + 边界边记录，是 `prove_chunk` 的输入单元 |
| claim | 对某 MLE 多项式在一个求值点上的值承诺：`Claim { point, eval }`；证明按 claim 反向传播 |
| LogUp / sumcheck | 查表证明的两组交互式证明协议；einsum 走 sumcheck，查表算子走 LogUp + linking sumcheck |
| MLE | 多线性扩展多项式；张量 pad 到 2^n 后即视为其 MLE 在超方格点上的取值表 |

### 1.5 参考资料

| 编号 | 文件 |
| --- | --- |
| R1 | `zkml/src/model/mod.rs` |
| R2 | `zkml/src/model/llm.rs` |
| R3 | `zkml/src/model/trace.rs` |
| R4 | `zkml/src/tensor/handle.rs` |
| R5 | `zkml/src/layers/transformer/mod.rs` |
| R6 | `tenstore/src/genstore.rs` |
| R7 | `zkml/src/layers/provable/mod.rs`（v1.1） |
| R8 | `zkml/src/graph/graph.rs`（v1.1） |
| R9 | `zkml/src/iop/prover.rs`（v1.1） |
| R10 | `zkml/src/iop/chunking.rs`（v1.1） |
| R11 | `zkml/src/iop/prover_graph.rs`（v1.1） |
| R12 | `zkml/src/lookup/context.rs`、`zkml/src/lookup/operation/generic_prove.rs`（v1.1） |

---

## 2 总体设计

### 2.1 架构概览

Runner 栈采用装饰器（洋葱）结构：外层 Runner 负责横切关注点（内存生命周期、记账、存储），只有芯层（BaseRunner）真正执行模型算子。分层如图 1 所示。

```text
图 1  Runner 洋葱栈分层

        调用方向（进入）
        ═══════════════▶
┌───────────────────────────────────────────────┐
│  HandleLifetimeRunner    内存生命周期 + 调度适配（壳）│
│  ┌───────────────────────────────────────────┐ │
│  │  TraceRunner         Trace 记账            │ │
│  │  ┌────────────────────────────────────────┐│ │
│  │  │  StoreRunner       后台线程写盘        ││ │
│  │  │  ┌─────────────────────────────────────┐││ │
│  │  │  │  BaseRunner     执行 layer.evaluate │││ │
│  │  │  │  （burn 后端：CPU 标量 / GPU kernel）│││ │
│  │  │  └─────────────────────────────────────┘││ │
│  │  └────────────────────────────────────────┘│ │
│  └───────────────────────────────────────────┘ │
└───────────────────────────────────────────────┘
        ◀═══════════════
        返回方向（RunOutput 逐层回穿，各层各取所需）
```

### 2.2 组件清单与职责划分

| 组件 | 层级 | 核心职责 | 典型副作用 |
| --- | --- | --- | --- |
| `HandleLifetimeRunner` | 壳 | 输入供给适配（按图查上游 key）、引用计数、内存即时回收 | dry 过期输入；维护 `storagekey_to_handle` |
| `TraceRunner` | 中层 | 按节点记录 `TraceStep`、收集模型输出 handle、组装 `Trace` | 修改 `Trace` 结构 |
| `StoreRunner` | 中层 | 把输出/proving 数据卸载到后台线程写 store | spawn 线程；异步编码 + 落盘 |
| `BaseRunner` | 芯 | 输入懒加载、执行 `layer.evaluate()`、输出打包为 handle、proving handle 构造 | 真正的后端计算 |

**分层顺序的约束**：StoreRunner 必须位于 TraceRunner 与 HandleLifetimeRunner 之下（即先于它们接收调用）。源码注释（mod.rs:1605-1606）明确原因：后两者会对张量做 dry，若顺序颠倒，写盘侧可能读到已被清除的数据。

### 2.3 Runner 栈的组装

栈由 `Model::run_with_split_nodes_info`（mod.rs:1587-1625）组装，顺序**从芯到壳**：

```rust
let runner = BaseRunner { store, split_nodes_info };          // 芯
#[cfg(test)] let runner = SanityCheckRunner { inner: runner };// 仅测试：形状健全性检查
let runner = StoreRunner::new(runner, store.clone());         // +1：后台写盘
let runner = TraceRunner::new(runner, input_handles.clone()); // +2：trace 记账
let mut runner = HandleLifetimeRunner::new(runner, &self.graph); // 壳
```

组装完成后即进入驱动循环（见 4.1）。run 结束时依次解构：`runner.into_inner()` 取回 TraceRunner，`into_parts()` 触发 `build_trace_outputs()` 并取出 `Trace`（mod.rs:1613-1622）。

**对照组**：无 trace 需求的纯推理走 `Model::run_float`（mod.rs:1628-1643），栈仅 `HandleLifetimeRunner(BaseRunner)` 两层——不 spawn 后台线程、不产出 `Trace`。

### 2.4 节点调用的穿行模型

`Model::run_with_runner`（mod.rs:1552-1585）按拓扑序遍历图节点，对每个节点执行一次完整的洋葱穿行：

```rust
runner.model_inputs(&self.graph, &inputs)?;
for (node_id, layer) in self.graph.forward_inners() {
    runner.run_layer(node_id, &self.graph, layer, &())?;
}
```

- 进入方向：外壳 → 中层 → 芯；
- 返回方向：`RunOutput { outputs, proving_data, tracked_data, trace_split_info }` 沿原路回传，**各层在返回途中完成自己的横切动作**（发数据进通道 / 记 step / 减引用计数并 dry）；
- 该模式对 LLM 多 token 场景的适配见 4.3。

---

## 3 组件详细设计

### 3.1 LayerRunner 接口契约

`LayerRunner<N, I>`（mod.rs:118-145）约定两个钩子：

| 钩子 | 调用时机 | 说明 |
| --- | --- | --- |
| `model_inputs(graph, inputs)` | 每次模型推理开始时调用一次 | LLM 场景下等价于"每个 token 轮一次"，是跨轮状态重置的约定入口 |
| `run_layer(node_id, graph, layer, inputs)` | 每个图节点调用一次 | `inputs` 的类型由第二个泛型参数 `I` 决定（`RunInput<N>` 或 `()`），用于区分"接收输入"与"自取输入"两种模式 |

### 3.2 BaseRunner

**位置**：mod.rs:147-246。**输入模式**：`RunInput<N>`（mod.rs:125-131，显式接收输入 handle 数组）。

`run_layer`（mod.rs:173-246）的动作序列：

1. **输入懒加载**：对每个输入 handle 调 `handle.wrapped_tensor()?`（handle.rs:386-410）。若 handle 已被 dry，触发 `store.fetch(storage_key)`：LRU 未命中则读页文件 → msgpack 解码 → 重建张量。此为"从磁盘重载 trace"路径的机制入口；
2. **输出形状预计算**：按 `PaddingMode::NoPadding` 预先计算输出形状。注释指明目的：concatenation cache 若被评估过程先行修改，会导致形状推导错误，故必须前置；
3. **算子执行**：`layer.evaluate(&wrapped_tensors)`，委托给 burn 后端（CPU：ndarray 核；GPU：cubecl kernel）；
4. **输出打包**：每个输出张量构造 `TensorHandle`，绑定 `port.to_storage_key()` 与 store 的克隆引用（mod.rs:209-217）。此后 handle 具备"key + 后备存储 + 可空副本"三件套语义；
5. **proving handle 构造**：`ProvingHandle::new(node_id 的 key, layer_out.proving_data, store)`（mod.rs:219-221），量化证明数据独立成 handle；
6. **可选切分**：若 `split_nodes_info` 存在（分布式 chunk 场景），调用 `trace_split.map_handles(...)` 对输出做分块重映射（mod.rs:223-234）。

### 3.3 StoreRunner

**位置**：mod.rs:454-666。**输入模式**：透传（`RunInput<N>`）。

**结构与线程模型**：

- 构造即 spawn 专用线程 + `mpsc::channel`（mod.rs:494-532）；
- 推理线程只做 clone + send，编码与写盘全部在后台线程完成——这是"写盘不占推理关键路径"这一实测结论（trace 生成热点中无任何盘 I/O 符号）的机制根源。

**发送规则**（`run_layer`，mod.rs:600-673）：

| 发送对象 | 条件 | 方法 |
| --- | --- | --- |
| 输出 handles | 无条件 | `send` |
| split 产出的 chunk handles | `trace_split_info.output_handles` 非空 | `send` |
| 原（未切分）handle | 存在 recombination layer，或该输出同时是模型输出 | `send` |
| split layer handles | `trace_split_info.split_layer` 存在 | `send` |
| proving handles | 未 split：原 proving handles；已 split：每 chunk 的 proving handles | `send_element` |
| 模型输入 handles | `trace_split_info.model_input_handles` | `send` |

**发送数据逐项拆解——每类数据是什么、给谁用、为什么必须有**（v1.1 补充）：

发送循环的主干是"输出 × 出边 feed"配对展开（mod.rs:625-665）：

```rust
for (original_handle, feed) in layer_out.outputs.iter().zip(graph.outgoing_feeds(node_id)) { ... }
```

注意配对键是 `outgoing_feeds`（展平的 Feed 列表，按 port 排序）而非 `outputs` 索引本身——`outputs[i]` 的语义由第 i 个**出边端口**定义（见 §3.7）。逐项拆解如下：

| # | 数据 | 触发条件 | 消费方（谁最终会读它） | 为什么需要（缺了会怎样） |
| --- | --- | --- | --- | --- |
| 1 | 输出 handle（`send` 原始 handle） | 无条件——每个有出边的节点 | ① 下游节点的懒加载（HandleLifetimeRunner 计数归零后 dry，下游再触发 store 重载）；② prove 阶段 `trace_step.output_tensor_at(port)` 的数据源 | 这是全图数据流的落盘基线。不发送则任何"内存副本被回收"或"跨进程 prove"场景下，该中间量永久不可达，Trace 退化为无肉的骨架 |
| 2 | split 产出的 chunk handles | 该节点被 chunk 切分（`trace_split_info.output_handles` 命中该 storage_key） | 各 chunk 的 `ChunkProver`：每个 chunk 只拿自己覆盖的输入切片 | 切分后每 chunk 需要独立的张量分片落盘；不发则分布式/分块证明读不到分片 |
| 3 | 原（未切分）handle | ① 有 recombination layer；② 该输出同时是模型输出 | ① recombination step 的证明需要"分片前的完整张量"做合并校验；② Trace.outputs 需要**未分块**的最终输出（to_verifier_io 直接消费） | recombination 若只存分片，合并结果的正确性无从证明；模型输出若只存分片，`trace.outputs` 无法重组完整 logits |
| 4 | split layer handles | `trace_split_info.split_layer` 存在 | split 层的 lookup witness 与 boundary claims 证明 | split 层是切分的"缝合点"，其自身输入输出必须可独立取数 |
| 5 | proving handles（`send_element`，i64） | 无条件；split 时改为逐 chunk 的新 proving handles | 每个算子的 prove 协议：softmax 的 shift_tensor、layernorm 的 mean/std_dev、logits 的 max_values 等都从 `ProvingHandle` 处取数（§5.3） | 证明需要的是**量化前辅助量**（如 softmax 的最大值表项），这些量不会出现在普通输出张量里；不发则对应算子无法生成 lookup witness |
| 6 | 模型输入 handles | `trace_split_info.model_input_handles` | Trace.inputs 与首 chunk 的 Input 节点播种 | 输入是整条 claim 链的起点；缺输入则首层 input claim 无从校验 |

三条设计原理贯穿上表：

- **原则 A（一切以 prove 阶段可读为准）**：发送集合 = prove 阶段所有 `get_step → output_tensor_at / input_tensors` 触点（§5.4）的闭包。多发的代价只是磁盘，少发的代价是证明失败；
- **原则 B（普通输出与切分输出正交）**：切分路径（#2/#3/#4）全部由 `trace_split_info` 驱动；未切分节点走 #1 的直发分支（mod.rs:649-651 的 else 分支），两套逻辑互不干扰；
- **原则 C（proving 数据独立通道）**：`send_element` 与 `send` 是同一 store 中的不同消息变体（mod.rs:454-479），实际存储行为一致，区分仅为类型层面的 i64 定型——proving 数据只存在于量化域（Element=i64），而输出 handle 可能是 f32 域（run_float 场景）。

**接收侧**（后台线程）：循环 `rx.recv()` → `wrapped_tensor.get_data()`（GPU 后端时阻塞同步并回传主机）→ `store.store(key, data)`（msgpack 编码 + 文件写 + LRU 插入）。

**收尾保证**（`Drop`，mod.rs:640-656）：释放发送端使通道关闭 → 线程排空剩余消息 → `join()`。语义：**run 函数返回时，全部数据已落盘**——这是后续读回与分布式 re-hydrate 正确性的前提。

**与 dry 的相容性**（源码注释 mod.rs:499-505）：dry 仅删除 handle 内共享的 `WrappedTensor` 副本引用；StoreRunner 线程持有的 clone 具有独立引用计数，故 dry 不会使后台线程拿到空数据。代价是相关显存/内存的释放被推迟到后台线程消费完毕。

### 3.4 TraceRunner

**位置**：mod.rs:796-891。**输入模式**：透传。

**构造**：`Trace::new(input_handles)`——模型输入直接成为 `trace.inputs`。

**run_layer**（mod.rs:855-889）在 inner 返回后：

1. **记账**：`trace.new_steps_for_splitted_nodes(node_id, inputs, outputs, split_info, proving_data)`（trace.rs:150）——每个图节点产出一个或多个 `TraceStep`（节点 id、输入 handles、输出 handles、proving data）。被 split 的节点产出 chunk steps 与 recombination step；
2. **模型输出收集**：扫描该节点下游 feed，若目标节点是模型输出节点，则将对应输出 handle `into_dry_tensor()`（脱水为纯 handle）登记进 `model_output_handles[output_idx]`（mod.rs:864-886）。

**收尾**（`into_parts` → `build_trace_outputs`，mod.rs:828-849）：断言输出索引从 0 开始且连续无缺，随后写入 `trace.output`。

**产物语义**：`Trace { inputs, steps, outputs }` 中，steps 数等于图节点数（GPT-2 为 259），所有张量本体驻留 store（GPT-2 为 347 pages）。Trace 记录"形"（引用结构），store 承载"肉"（数据）——证明阶段要么就地消费（同进程，LRU 命中），要么跨进程 re-hydrate。

### 3.5 HandleLifetimeRunner

**位置**：mod.rs:902-1105。**输入模式**：`()`（外壳不接收输入，自行按图供给）。

**构造**（mod.rs:920-958）：扫描全图 edges，建立 `handle_usage_count`（storage_key → 被消费次数）；对模型输出节点的消费**预先 +1**，确保输出张量的计数永不递减到 0——输出必须在 run 结束后存活。

**model_inputs**（mod.rs:1002-1044）——LLM 每个 token 轮均触发：

1. 对全部 edges 重新累加计数（**跨轮刷新点**；源码注释：LLM 多轮 run 依赖 `model_inputs` 重建计数、句柄在每层运行后被覆盖）；
2. 未被任何层使用的输入立即 dry 并告警；
3. 输入 handle 注册进 `storagekey_to_handle`。

**run_layer**（mod.rs:1046-1096）：

1. **输入供给适配**：按 `graph.incoming_feeds(node_id)` 获取上游 storage_key，从 registry 克隆 handle，组装 `RunInput` 传给内层。"某层的输入是什么"由本层统一裁决，内层零感知；
2. **即时回收**：执行完成后对本层每个输入 key 计数 −1，递减到 0 立即 `dry()`——释放内存/显存副本（store 数据仍在，可重载）；
3. **输出登记**：输出 handles 全部写入 registry；LLM 多轮 run 复用同一批 storage_key 时直接覆盖（mod.rs:1084-1090 注释：覆盖为合法行为）。

**净效果**：推理过程的常驻内存约等于**活跃的中间张量**之和，而非全图张量总量——这对长上下文（如 ctx=1024 的 attention/KV 中间量）与 GPU 显存预算至关重要。

### 3.6 LayerOut 与 layer.evaluate 内部机制

#### 3.6.1 Evaluate trait 与 LayerOut 三件套

`Evaluate` trait（provable/mod.rs:308-310）只有一个方法：

```rust
pub trait Evaluate<T: TensorTypeParam> {
    fn evaluate(&self, inputs: &[&WrappedTensor<T>]) -> Result<LayerOut<T>>;
}
```

每个层对 `f32`（浮点推理域）与 `Element`（i64，量化证明域）各实现一次；BaseRunner 在量化模型上调用的是后者。返回值 `LayerOut<T>`（provable/mod.rs:207-214）是本节的核心——**层评估一次性产出三路数据，分别服务三个下游消费者**：

```text
图 2  LayerOut 三件套与其消费路径

                        ┌─ outputs ──────────▶ BaseRunner 打包成 TensorHandle
                        │                      （storage_key + store 引用 + 副本）
layer.evaluate() ──▶ LayerOut ─ proving_data ──▶ ProvingHandle::new(...) 落盘
                        │                      （证明期由各算子 prove 协议取用）
                        └─ tracked_layer_data ─▶ RunOutput.tracked_data
                                               （仅 TrackerRunner 在量化校准时消费；
                                                正常推理路径中随 RunOutput 一起被丢弃）
```

| 字段 | 类型 | 服务对象 | 生命周期 |
| --- | --- | --- | --- |
| `outputs` | `Vec<WrappedTensor<T>>` | 图数据流（下游层输入 + trace 输出） | 与张量生命周期一致 |
| `proving_data` | `ProvingData`（评估期）/ `ProvingHandle`（运行期） | 证明协议 | 落盘，prove 阶段取回 |
| `tracked_layer_data` | `HashMap<TrackedDataId, WrappedTensor<T>>` | 量化校准器 | **仅校准运行中存活**，正常推理被丢弃 |

`ProvingData`（provable/mod.rs:44-60）是证明辅助数据的枚举，六个变体逐一对应"证明该算子时除输入输出外还必须固定的中间量"：

| 变体 | 承载数据 | 产生算子 | 证明期用途 |
| --- | --- | --- | --- |
| `Convolution(ConvFFTData)` | FFT 卷积中间量 | Conv | 卷积证明的频域一致性 |
| `Softmax(SoftmaxData)` | LUT + shift tensor | Softmax | LogUp 查表证明的表与移位校验（§5.3） |
| `LayerNorm(LayerNormProvingData)` | mean / std_dev | LayerNorm | 归一化查表证明的权重张量与统计量 |
| `RMSNorm(RMSNormProvingData)` | RMS 统计量 | RMSNorm | 同上（RMS 变体） |
| `ArgMax(ArgmaxData)` | `max_values`（每行最大值序列） | Logits | argmax 专用协议的比较链证明 |
| `None` | — | einsum / requant 等无需额外数据的算子 | — |

`ProvingHandle`（provable/mod.rs:66-71）是运行期包装：`ProvingHandle::new(storage_key, proving_data, store)`（BaseRunner，mod.rs:219-221）把 `ProvingData` 中的每个张量登记为独立 handle 落盘，prove 阶段按 node_id 的 storage_key 取回。StoreRunner 对它走 `send_element`（§3.3 #5）。

#### 3.6.2 各算子的 evaluate 行为剖析

以下按 GPT-2 路径上的代表性算子给出 evaluate 内部在做什么（全部为 `Evaluate<Element>` 量化域实现）：

**（a）EinSum**（einsum/mod.rs:303-309）——最"薄"的实现：

```rust
fn evaluate(&self, inputs: &[&WrappedTensor<N>]) -> Result<LayerOut<N>> {
    let outputs = self.evaluate_internal(inputs)?;
    Ok(LayerOut::from_vec(outputs))
}
```

`evaluate_internal`（einsum/evaluate.rs）委托 burn 后端做张量收缩（matmul/bmm）。**不产生 proving_data、不产生 tracked data**——einsum 的证明（sumcheck，§5.3）只需要输入输出张量本身，全部可从 trace 恢复，因此 `ProvingData::None`。这解释了为什么 einsum 是 trace 中体积最大但证明辅助数据最少的算子。

**（b）Softmax**（softmax/evaluate.rs）：

1. 校验已量化（`quant_info.is_some()`）且单输入；
2. 计算逐行最大值并构造 `SoftmaxData`（含 LUT `lut` 与 `shift_tensor`）——量化 softmax 的核心技巧是"先减最大值再查表"，减最大值的移位量必须**在评估期固定下来**供证明期复算校验；
3. `LayerOut::from_vec(outputs).with_proving_data(ProvingData::Softmax(data))`。

消费路径：`gen_lookup_witness`（softmax/lookup.rs:124）在 prove 阶段用 `SoftmaxData` 生成 LogUp 多重性多项式；`prove_internal`（softmax/prove.rs:11）做 LogUp + linking sumcheck + shift check。

**（c）LayerNorm**（layernorm/evaluate.rs:91 起，`evaluate_quantised_internal`）：

纯 i64 定点手写实现（不走 burn）：

1. `n_mean = input.sum_dim(-1)`——沿归一化维求和得均值分子；
2. `float_converter = (m_scale²) / (dim_size³)`——三个 scaling 因子的定点换算常数，均值/方差的量化标度在此确定；
3. `subbed_input = input*dim − n_mean`、`normalising_factor = subbed²·…`——中心化与方差估计全部用 mul/mul_scalar 定点运算展开；
4. 产出 `LayerNormProvingData { mean, std_dev, … }` 并 `with_proving_data(ProvingData::LayerNorm(...))`。

要点：**均值与标准差是评估期算出、证明期只校验的量**——证明协议（LogUp 查表 + 权重张量）无法从输入输出反推这两个中间统计量，故必须随 trace 固定。这正是 proving_data 存在的必要性证明。

**（d）Activation**（activation.rs:434-466）——tracked_layer_data 的主要生产者：

```rust
Ok(LayerOut::from_vec(vec![activation_out.clone()]).with_data_to_be_tracked(
    HashMap::from([
        (layer.tracked_output_data_id().into(), activation_out),
        (layer.tracked_input_data_id().into(), inputs[0].clone()),
    ]),
))
```

激活函数（GELU/ReLU/SiLU 等）把**自己的输入与输出**登记进 tracked_layer_data，id 由层实例生成（activation.rs:70/78：`tracked_input_data_id` / `tracked_output_data_id`）。GLU 变体（activation.rs:452 起）额外对门控分支做同样登记。这组数据不走 proving_data 通道，因为它不参与本层证明——它服务于**量化校准**（见 3.6.3）。

**（e）Logits（argmax）**（logits.rs:382-394）：

```rust
let (output, argmax_data) = self.evaluate_with_argmax_data_element(inputs)?;
let argmax_data = ArgmaxData { max_values: argmax_data };
Ok(output.with_proving_data(ProvingData::ArgMax(argmax_data)))
```

argmax 层在评估期记录每行的最大值序列 `max_values`；证明协议（logits.rs:519 起）用比较链证明"输出索引处确为最大值"，该协议需要逐行最大值本身作为 witness，无法从输出张量反推。

**（f）其余算子速览**：Requant 只做移位校验（shift check），无需 proving_data；Embeddings/Positional 走查表，数据来自常量权重而非评估期中间量；AttentionMask 走查表（attention_mask/mod.rs:299 起 prove）。

#### 3.6.3 tracked_layer_data 深挖：量化校准通道

**它是什么**：`HashMap<TrackedDataId, WrappedTensor<T>>`（provable/mod.rs:204,213），键为字符串 id。它是层评估的**旁路输出**——与 outputs（数据流主线）、proving_data（证明数据）并行的第三条通道。

**谁消费它**：全代码库中唯一的消费者是 `TrackerRunner`（mod.rs:669-713，量化阶段临时入栈的 Runner）：

```rust
// mod.rs:710
for (data_id, handle) in layer_out.tracked_data.drain() {
    self.tracker.track_intermediate_data(node_id, data_id, handle);
}
```

`track_intermediate_data`（quantization/strategy/llm_tracker.rs:159-170）把张量喂给三类累加器：

| 累加器 | 键 | 用途 |
| --- | --- | --- |
| `intermediate_data_trackers` | `(NodeId, TrackedDataId)` | 逐节点逐数据的统计（min/max/均值），用于确定该节点输出的量化标度 |
| `global_data_trackers` | `TrackedDataId` | **跨节点全局统计**——典型用途是激活函数：同一激活（如 GELU）在多处出现，其查表范围（table 的 min/max）必须取所有出现位置输入/输出的全局极值（llm_tracker.rs:26-27 注释明说"for working out suitable tables"） |
| `accumulators` | `NodeOutput` | 节点输出本身的统计（HandleLifetimeRunner 同款编址） |

**为什么需要这条独立通道**：量化策略推导（尤其 LLM 的 activation 表定界）需要的是**前向过程中的真实激活分布**，而这些中间量要么不在任何层的输出里（如激活函数的输入），要么散布在全图各处需要跨节点聚合。把"校准所需数据"作为层的自描述元数据（层自己声明 track 什么、用什么 id），量化框架就能零耦合地收集——新增算子只需实现 `tracked_*_data_id`。

**正常推理时它去哪了**：BaseRunner 把它原样放进 `RunOutput.tracked_data`（mod.rs:240），但正常栈中 StoreRunner/TraceRunner/HandleLifetimeRunner 都不读这个字段，`RunOutput` 在每轮循环末尾被整体丢弃。即：**tracked_layer_data 是一次性校准数据，运行时成本为零拷贝传递、终局成本为零持久化**（drain 语义还确保不会重复统计）。

**与 proving_data 的边界**：两者容易混淆，判别标准是"证明该算子是否需要"——softmax 的 max 是证明需要的（proving_data），GELU 的输入输出是校准需要的（tracked_layer_data）。一个层可以两者都有（如某些激活的量化变体）。

### 3.7 Port 与 Feed：图连接与寻址机制

#### 3.7.1 编址体系

模型图的所有连接都通过"节点 id + 端口号"编址（graph.rs:213-235）：

| 类型 | 定义 | 语义 |
| --- | --- | --- |
| `NodeId(usize)` | 拓扑序号（display 为 `N{x}`） | 节点唯一标识 |
| `NodeOutput(NodeId, port)` | 某节点的第 port 个输出（display 为 `N{x}_{port}`） | 数据流的**源**端点 |
| `NodeInput(NodeId, port)` | 某节点的第 port 个输入 | 数据流的**汇**端点 |
| `PortId` | 端口号的强类型包装 | 多输入算子（如 GLU 双输入、attention 的 Q/K/V）靠 port 区分 |

编址的**串联机制**是一条纯格式化链：

```text
图 3  从图连接到磁盘寻址的完整链条

  Edge { source: NodeId, target: NodeId,
         ports: Vec<link { source_port, target_port }> }   graph.rs
      │  一条边可携带多个端口连接（多输出扇出）
      ▼
  Feed { source: NodeOutput(N5,1), target: NodeInput(N9,0) }   graph.rs:518-556
      │  incoming/outgoing_feeds: 把边按 port 排序展平成 Feed 列表
      ▼
  NodeOutput.to_storage_key() = format!("N5_1")                mod.rs:1107-1112
      │  字符串化 = store 寻址键
      ▼
  GenStore page "N5_1" ──▶ handle.load() ──▶ 下游层输入
```

关键点：**端口号同时承担两个角色**——图中"第几个输出/输入"的拓扑角色，以及 store 中"哪个文件页"的寻址角色。二者靠 `format!("{self}")` 这一格式化约定绑死，因此 INV-8（端口稳定性）成立。

#### 3.7.2 outgoing_ports 在做什么

`outgoing_ports(n)`（graph.rs:882）：

```rust
pub(crate) fn outgoing_ports(&self, n: NodeId) -> Vec<NodeOutput> {
    // 收集 n 的所有出边上的 link.source_port → n.output_at(port)
    // 按 port 数值排序，去重
}
```

逐层拆解：

1. 遍历以 n 为 source 的全部边；
2. 对每条边的每个端口连接取 `source_port`，构造成 `NodeOutput(n, port)`；
3. **按 port 排序并去重**后返回。

为什么需要排序去重：边的存储顺序由图构建过程决定（不保证 port 升序），而下游消费方（BaseRunner）需要把 `layer_out.outputs[i]` 与"第 i 个输出端口"对齐——这个对齐关系只能靠 port 数值序建立。去重处理的是多边共享同一输出端口（扇出）的情况：一个输出喂多个下游时，它在端口列表里只应出现一次。

BaseRunner 的消费（mod.rs:209-217）：

```rust
let out_ports = graph.outgoing_ports(node_id);
for (port, out_shape, tensor) in izip!(&out_ports, &out_shapes, layer_out.outputs) { ... }
```

三者 izip：**第 i 个输出张量 × 第 i 个输出端口 × 第 i 个输出形状** → `port.to_storage_key()` 作为 handle 的 key。这就是"层输出的顺序语义"的定义点：层实现者按 `OpInfo::output_shapes` 的约定顺序产出，图按端口序收货。

#### 3.7.3 feeds 家族与各 Runner 的分工

| 函数 | 位置 | 返回 | 消费者 |
| --- | --- | --- | --- |
| `outgoing_ports(n)` | graph.rs:882 | `Vec<NodeOutput>`（排序去重） | BaseRunner：outputs → storage_key |
| `outgoing_feeds(n)` | graph.rs:538 | `Vec<Feed>`（按 port 排序展平） | StoreRunner：输出 × feed 配对发送（§3.3）；TraceRunner：模型输出收集 |
| `incoming_feeds(n)` | graph.rs:518 | `Vec<Feed>`（按 port 排序展平） | HandleLifetimeRunner：查上游 storage_key 组装 RunInput |
| `output_at(port)` / `input_at(port)` | graph.rs:226-235 | 单端点构造器 | 各处的端点字面量构造 |

三个 Runner 分别消费 feeds 家族的三个投影，形成完整闭环：

```text
图 4  feeds 在洋葱栈中的消费闭环

HandleLifetimeRunner ── incoming_feeds ──▶ "我从哪拿输入"（查 registry / store）
BaseRunner           ── outgoing_ports ──▶ "我的输出叫什么 key"（izip 定位）
StoreRunner          ── outgoing_feeds ──▶ "我的输出喂给谁"（含"是否模型输出"判断）
```

**为什么 StoreRunner 用 feeds 而 BaseRunner 用 ports**：BaseRunner 只关心"我的第 i 个输出是什么 key"——ports 足矣；StoreRunner 还要判断某输出是否直连模型输出节点（mod.rs:653-663，决定是否为 trace.outputs 额外保存原值）——这需要 Feed 的 target 端信息，ports 里没有。两条 API 的信息密度差异正是分工的依据。

**不变式支撑**：该机制的正确性依赖"端口号与层输出顺序一一对应且全图稳定"，登记为 INV-8（§6）。

---

## 4 关键流程

### 4.1 模型图执行流程

单次 `run_with_runner` 的执行模型：

1. **拓扑序单遍**：`graph.forward_inners()` 给出节点执行序列（transformer 展开后 259 节点），依序驱动，无递归、无重放；
2. **数据流隐式衔接**：上游输出 handle 注册进 HandleLifetimeRunner 的 registry；下游由外壳按 feed 查表供给输入，运行时不存在额外的中间缓冲搬运；
3. **三线并行**：每个节点在"返回路径"上同时触发三件事——
   - 计算继续（下一节点开始 evaluate）；
   - 数据落盘（StoreRunner 已发通道，后台线程异步消费）；
   - 内存释放（输入计数归零 → dry）。

   主线程内这三者只表现为"append 到 channel / 减计数"，真正的编码、写盘与设备同步均发生在旁路线程或返回间隙。

### 4.2 Trace 生成流程

`run_with_split_nodes_info` 的完整数据流：

```text
图 5  Trace 生成流程

tokens ─▶ tensor_to_handles ─┬─▶ Trace.inputs
                             │
   拓扑序逐节点 ─────────────▶│
   ├ BaseRunner.evaluate ──▶ 输出 handle
   ├ TraceRunner: new_steps_for_splitted_nodes ─▶ steps[]（259）
   └ StoreRunner: 后台线程 store.store ─▶ pages（347）
                             │
   收尾: build_trace_outputs（outputs 为 dry handle）
        + attach_split_info（分布式）─▶ Trace{inputs, steps, outputs}
```

设计要点：

- **Trace 与数据解耦**：`Step` 仅含 handle 引用，全部张量本体在 store。该结构决定了两种持久化方案均可行——整 Trace 单文件序列化（数据内嵌），或依赖 store 重载；
- **一致性由执行序保证**：步骤按拓扑序记录，proving data 与产生它的节点绑定；
- **分布式分支**：`split_nodes_info` 有值时，BaseRunner 做 handle 重映射、StoreRunner 对 chunk handles 与 recombination 原值分别发送（未切分节点走原 handle 直发，mod.rs:625-665）、TraceRunner 用同一信息生成 chunk steps；收尾由 `attach_split_info` 补齐。

### 4.3 Token 生成流程

`Driver::run_elements_with_runner`（llm.rs:417-625）。

**循环结构**（llm.rs:495-547）：

```text
图 6  Token 生成时序

prompt [L tokens]
  │
  ├─ run_#0: 全图前向 [L] ──────────── logits[L] ─ token₁ ┐（层内 cache 未命中）
  ├─ run_#1: 全图前向 [1] ─ +cache ─→ logits    ─ token₂ ┤（增量步）
  ├─ …                                                   ├─→ 结果线程: eos? → stop
  └─ run_#k: [1] ─→ token_{k+1} ─────────────────────────┘
  │
  └─ heavy: run_with_split_nodes_info([L+k+1 tokens]) ─▶ Trace ─▶ prove
```

**增量机制澄清**：外层循环不是外置 KV cache 的"一步一喂"——每轮 `run_with_runner` 都完整重放 259 节点。增量来自**层内 concatenation cache**（transformer/mod.rs:19-117）：cache 层持有 `cached_tensor`，输入 [1] token 时先 `cat([cached, new], dim)` 补全到当前序列长度再计算。因此第一轮输入 [prompt]（全量），之后每轮输入 [1]（增量）。实测耗时分布与此吻合：首轮全量前向最贵，增量轮显著更快。

**结果线程与 EOS**（llm.rs:469-494）：`mpsc::sync_channel(10)` 起独立结果线程，接收每轮的末位 token；命中 `eos_token` 则置 `stop`，循环在下一轮开头退出；否则追加到 `generated_tokens`。通道容量 10 同时起到背压限流作用：推理线程持续调度以维持 GPU 硬件占用（源码注释），结果处理不阻塞调度。

**收尾与 heavy run**（llm.rs:549-625）：

1. `drop(tx)` → 结果线程返回，且**去掉最后一个 token**（最终重跑会重新生成它）；
2. `full_sentence = prompt + generated`；
3. `reset_selective(&[softmax cache])`（llm.rs:561）——清除层内 cache，保留其余模型状态；
4. **heavy run**（llm.rs:566-573）：将完整 sentence 作为单次输入执行 `run_with_split_nodes_info`——一次**无 cache 重放的干净完整前向**，产出正式 `Trace`。此为 trace 的最终生成点。

### 4.4 一致性保证

heavy run 之后执行两组断言（llm.rs:574-640），作为"增量 cache 前向与全量前向数值等价"的运行时校验：

| 断言 | 内容 | 意义 |
| --- | --- | --- |
| 移位校验 | `trace.outputs[0][i-1] == trace.inputs[0][i]`，对全部生成区间 | 全量前向的输出应与输入的因果移位一致 |
| argmax 校验 | 若移位校验失败，取 argmax 步的 logits 行做 top-1 复核 | 区分"数值错误"与"top-1 并列引发的合法歧义"，断言信息含该行排序后的前 10 值 |

---

## 5 prove_chunk：Trace 的消费与证明全流程

前四章回答"trace 从哪来"，本章回答"trace 到哪去"——`Prover::chunked_prove_local`（prover.rs:1043）如何把 heavy run 产出的 `Trace<Element>` 切成 chunk、逐 chunk 消费、最终拼成整模型证明。入口在 LLM driver：`LLMDriver::prove`（llm.rs:774）→ `chunked_prove_local`。

### 5.1 从 Trace 到 chunk：切分策略与执行图

#### 5.1.1 chunk 切分策略

`ChunkingStrategy`（chunking.rs:969-986）约定两个方法：`ideal_num_chunks`（决定切多少块）与 `split`（实际切分）。两种实现：

| 策略 | 位置 | 切分逻辑 | 适用场景 |
| --- | --- | --- | --- |
| `DefaultChunkingStrategy` | chunking.rs:1000 起 | 简单按连续节点切，约 3 节点/chunk | 通用 CNN 类模型 |
| `LLMChunkingStrategy` | chunking.rs:1710 起 | **按 Add 节点切分**：Add 是残差汇合点，必须整体落在同一 chunk（否则 Add 的两个输入分属不同 chunk，需要额外合并证明）；同时允许 einsum/logits **横向切分**（大矩阵乘按行切给多个 chunk 并行证明） | Transformer：attention 层多、einsum 体积大 |

LLM 策略的设计权衡（chunking.rs:1704-1709 注释）：Add 整块化避免多重承诺，代价是 attention 层不能跨 chunk 切——只要 attention 层数够多、chunk 数够用，这个代价可接受。

切分产物 `ModelChunk`（chunking.rs）包含：节点子图（`subgraph`，含 SplitLayer/RecombinationLayer 缝合节点）、`incoming_edges` / `outgoing_edges`（跨 chunk 边界记录）、`chunk_id`。切分同时对 trace 做对应切分：`chunk.chunk_trace(&full_trace)`（chunking.rs:553）从全量 trace 中抽取本 chunk 覆盖节点的子 trace。

#### 5.1.2 证明执行图

`build_local_execution_graph`（prover.rs:939-1020）把"证明"本身组织成一张执行图，每 chunk 一个节点：

```text
图 7  证明执行图（每 chunk 一个 ChunkProver 节点）

full_trace ─▶ [SplitNode] ──chunk_trace_0──▶ [ChunkProver #0] ──┐
             （初始化 transcript、                 ……            ├─▶ [Final] ─▶ Proof
               squeeze 公共挑战；                 ……              │      （合并全部 chunk
               对每 chunk: chunk_trace + dry）──chunk_trace_n──▶ [ChunkProver #n] ┘        证明与 PCS opening）
```

- **SplitNode**（prover_graph.rs:158-190）：接收 `graph_inputs` 传入的唯一输入（full_trace），对每个 chunk 调 `chunk.chunk_trace(&full_trace)` 得子 trace；local 模式下 `disable_dry_trace` 关闭脱水（同进程无需释放），分布式模式则 dry 掉 chunk 用不到的 handle 以省内存；
- **ChunkProver**：对每个 chunk 调 `prove_chunk`（§5.2）；
- **Final**：收集全部 `ChunkProof`，做最终合并与 PCS opening，产出 `Proof`。

这一结构与模型推理图（§4.1）同构：推理图消费的是张量，证明图消费的是 trace。

### 5.2 prove_chunk 逐阶段拆解

`Prover::prove_chunk`（prover.rs:588-918）签名：`(chunk: ModelChunk, chunk_trace: &Trace<Element>, chunk_layers) -> ChunkProof`。九个阶段按序执行，其中 **trace 数据的消费点**已用 ▶ 标注：

**阶段 1 — transcript 绑定 chunk 身份**（prover.rs:600）
`chunk.add_chunk_data_to_transcript(self.transcript)`：把 chunk_id、边界结构写入 transcript，保证不同 chunk 的证明不可互换（防重放/拼接攻击）。

**阶段 2 — witness 批量承诺**（prover.rs:608，实现在 lookup/context.rs:224 `generate_lookup_witness_for_chunk`）
▶ 消费 trace：对 chunk 内每个节点取 `chunk_trace.get_step(node_id)`，调该算子的 `gen_lookup_witness(id, ctx, step_data)` 生成 LogUp 多重性多项式与表 witness；同时把 chunk 内各层输出张量 fieldize 后承诺（`chunk_commitments`）。
仪器化打点（prover.rs:1181-1188）：承诺前打印本 chunk trace 的元素数与字节数——G 文档 §4.5 的单 chunk 体积数据即来源于此。
边界边为空且无 lookup 上下文时直接返回默认值（context.rs:238-244）。

**阶段 3 — ChallengeStorage 初始化**（prover.rs:615-620）
从 transcript 派生查表挑战；无表时用空实现。

**阶段 4 — 模型输出 claims**（prover.rs:628-664）
▶ 消费 trace：`chunk.model_outputs_in_chunk()` 枚举本 chunk 产出的模型输出边 → 对每条边取 `trace_step.output_tensor_at(source_port)`（prover.rs:650）→ `to_field().pad_next_power_of_two()` → `compute_claim(transcript, tensor)`（手动求 MLE 在随机点的值，作为输出承诺）。
这是 verifier 可直接复核的"锚点 claim"：整条反向链最终都收敛到这些输出 claim 上。

**阶段 5 — 跨 chunk 出边 claims（`chunk_output_claims`）**（prover.rs:673-711）
▶ 消费 trace：对本 chunk 的全部 `outgoing_edges`，同样从 `trace_step.output_tensor_at` 取张量、fieldize、pad，然后**在公共挑战点 `common_point` 上求值**。公共点的长度取所有出边 MLE 变量数的最大值，不足则向 transcript 追加挑战补齐（prover.rs:697-705）——保证跨 chunk 的同名多项式在**同一点**上被求值，这是后续 verifier 校验 boundary 承诺一致性的前提。

**阶段 6 — 反拓扑逐节点证明（核心循环）**（prover.rs:717-853）
`chunk.subgraph.backward_iter()` 自输出端向输入端反向遍历，三类节点三种处理：

- **Output 节点**：播种——把阶段 4 的输出 claim 写入 `claims[NodeInput(node_id, 0)]`（prover.rs:848-853）。claim 登记表由此初始化；
- **Input 节点**：跳过（其 claim 由后继层产生后即构成对模型输入的约束）；
- **Inner 节点**（每个算子层）：
  1. ▶ 取 `chunk_trace.get_step(node_id)`（prover.rs:720-722），`section.output_tensors()` → `to_field().pad_next_power_of_two()`——**这是 trace 数据的主消费点**：该节点全部输出张量从 store（或内存）水合、转域、pad 成 MLE 取值表（prover.rs:750-753）；
  2. `chunk.claims_for_node(node_id, &claims, &chunk_output_claims)`：从登记表收集**下游邻居已登记的、指向本节点各输出端口的 claim**——即"我的输出被承诺了这些值"（prover.rs:762-763）；
  3. `flatten_and_merge_claims`：多个下游对同一输出端口的 claim 先做随机线性合并（prover.rs:768-770）；
  4. 按 `op.is_provable()` 分派（prover.rs:812）：
     - **可证算子**：`op.prove(node_id, ctx, claims_for_prove, section, prover)`（prover.rs:818）——各算子协议见 §5.3，返回"关于本节点**输入**的新 claim"；
     - **不可证算子**：直接透传 claim（prover.rs:823-829，注释：不可证层不改变输入值）；
  5. 把返回的输入 claim 按 `NodeInput(node_id, i)` 登记（prover.rs:840-846），成为上游节点在下一轮迭代中要满足的"债务"。

```text
图 8  claim 反向传播示意

  [Output claim 播种] ─▶ logits 层 prove ─▶ input claim (N258,0)
       ─▶ softmax 层 prove ─▶ input claim (N256,0)
       ─▶ einsum 层 sumcheck ─▶ input claims (N250,*) ─▶ …… ─▶ Input 节点
  每一步消耗下游 claim、产出上游 claim；trace 供给每层的 witness 张量
```

**阶段 7 — 边界 claims**（prover.rs:862-870）
`compute_output_boundary_edges_claims(&chunk_output_claims)`：对每条出边界边，把阶段 5 的 claim 与目标 chunk 中该多项式的**承诺 id** 绑定（commitment_id = SHA256(node, port) 前 8 字节，chunking.rs:700-712）；`compute_input_boundary_edges_claims(&claims)` 对称地处理入边界边。verifier 侧断言：同一 boundary 多项式在相邻两 chunk 中的承诺必须一致（chunking.rs:696-712 的 ensure）——跨 chunk 的数据流正确性由此保证，无需 chunk 间通信。

**阶段 8 — 表证明**（prover.rs:874-876）
`prove_tables(chunk_id, &lookup_ctx)`：对本 chunk 用到的每个 lookup 表生成"表本身是诚实的"证明（表行与挑战点求值）。

**阶段 9 — PCS opening 与打包**（prover.rs:885-918）
`commit_prover.prove`：对前面累积的全部承诺做最终打开（多多项式单点批量 opening）；连同 `steps`（逐层证明）、`merge_claim_proofs`（claim 合并证明）、`table_proof`、`chunk_data`（输出求值 + 承诺 + chunk 结构）打包为 `ChunkProof`。

### 5.3 逐算子证明协议

阶段 6 中 `op.prove()` 的分派表（协议粒度，数学细节不在本档范围）：

| 算子 | 协议 | 消费的 trace 数据 | 消费的 proving_data | 代码 |
| --- | --- | --- | --- | --- |
| EinSum | **sumcheck**（dp-crypto `IOPProverState`，对 einsum 表达式逐变量折叠） | 输入/输出张量（MLE） | 无（`ProvingData::None`） | einsum/prove.rs:41 起 |
| Softmax | **LogUp** 查表 + linking sumcheck + **shift check** | 输入张量 | `SoftmaxData`（LUT、shift_tensor） | softmax/prove.rs:11 |
| LayerNorm | LogUp 查表（带权重张量变体） | 输入张量 | `LayerNormProvingData`（mean/std_dev） | layernorm/prove.rs |
| RMSNorm | 同上（RMS 变体） | 输入张量 | `RMSNormProvingData` | rmsnorm/prove.rs |
| Activation | LogUp 查表 | 输入张量 | 无（表由量化期定界） | generic_prove.rs:66 |
| AttentionMask | LogUp 查表 | 输入张量 | 无 | attention_mask/mod.rs:299 |
| Logits(argmax) | **argmax 专用协议**（比较链 + max_values 承诺） | logits 输出 | `ArgmaxData.max_values` | logits.rs:519 |
| Embeddings | 查表 | 输入 token id | 权重为常量 | embeddings.rs:366 |
| Positional | 查表 | 输入 | 常量表 | positional/mod.rs:680 |
| Requant | **shift check**（移位溢出校验） | 输入张量 | 无 | requant/prove.rs |
| Hadamard | 乘积论证 | 输入/输出 | 无 | hadamard.rs:93 |
| Split / Recombination | 切分/合并一致性（claim 重映射与合并） | 分片张量 | — | prover.rs:823-829 透传分支 |

所有查表类算子共享同一骨架 `prove_lookup_op`（generic_prove.rs:66）：取阶段 2 承诺的层多项式 → 构造 LogUp 约束 → linking sumcheck 把"表内命中"与"输出 claim"绑定 → shift 校验（对 requant/softmax 的定点移位正确性）→ 返回输入 claim。

### 5.4 Trace 数据消费点汇总

trace 在 prove 阶段的全部消费触点（排重后）：

| 消费点 | 位置 | 读什么 | 用途 |
| --- | --- | --- | --- |
| SplitNode 切分 | prover_graph.rs:177 | full_trace 整体 | `chunk_trace(&full_trace)` 抽子 trace |
| witness 生成 | context.rs:224 起 | 每 step 的输入/输出 + proving handle | LogUp 多重性多项式、层输出承诺 |
| 输出 claims | prover.rs:628-664 | `output_tensor_at(port)` | 锚点承诺 |
| 出边 claims | prover.rs:673-711 | `output_tensor_at(source_port)` | 跨 chunk 公共点求值 |
| 反拓扑主循环 | prover.rs:750-753 | `section.output_tensors()` | 每层 prove 的 witness |
| verifier IO | trace.rs:311 `to_verifier_io` | trace.inputs/outputs | pad + fieldize 产出 `IO{input, output}` |

**结论**：trace 的消费模式是"**按 step 惰性水合、按算子协议取数**"——每个 chunk 只触碰自己的子 trace，每层只在 prove 时把所需张量从 store 拉回内存（local 模式下 LRU 命中即免 IO）。这解释了 G 文档的实测结论：prove 阶段的 IO 模式与 trace 生成时的 store 写入模式严格互补。

### 5.5 跨 chunk 一致性与 verifier 侧

**一致性三件套**（全部在无 chunk 间通信的前提下成立）：

1. **公共挑战点**：阶段 5 的 `common_point` 由共享 transcript 派生，所有 chunk 的出边 claim 在同一点求值；
2. **承诺 id 绑定**：boundary 多项式以 SHA256(node, port) 为 id，verifier 断言相邻 chunk 对同一 id 给出相同承诺（chunking.rs:696-712）；
3. **transcript 绑定**：chunk_id 与边界结构进 transcript（阶段 1），chunk 证明不可重排。

**verifier 侧**：`trace.to_verifier_io`（trace.rs:311-341）把 trace 的输入/输出 pad 到 2 的幂后转域，产出 `IO { input, output }`；`LLMVerifierContext::verify`（llm.rs:830-890）在通用证明校验之外追加两项业务校验：EOS 位置一致性与移位校验（生成区间的输出-输入因果移位，与 §4.4 的运行时断言同源）。

---

## 6 设计约束与不变式

以下不变式是上述机制正确性的隐含前提，修改任一组件时须重新核对：

| 编号 | 不变式 | 依据 |
| --- | --- | --- |
| INV-1 | dry 不得破坏后台写盘：dry 仅清除共享内部副本，StoreRunner 的克隆具有独立引用计数 | mod.rs:499-505 注释 |
| INV-2 | 模型输出 handle 的使用计数恒大于 0，不因引用耗尽被 dry | 构造时对输出节点预 +1（mod.rs:931-949） |
| INV-3 | storage_key 允许跨轮复用：LLM 多轮 run 的 handle 覆盖与计数刷新由 `model_inputs` 完成 | mod.rs:1002-1044、1084-1090 |
| INV-4 | 图输出的索引必须连续且从 0 开始 | `build_trace_outputs` 断言（mod.rs:828-836） |
| INV-5 | 推理一律使用 unpadded 张量，pad 只发生在 proving 层 | llm.rs:444 注释、BaseRunner 形状预计算约定 |
| INV-6 | run 函数返回时 store 已全量落盘 | `StoreRunner::Drop` 的排空 + join 语义 |
| INV-7 | 增量步（cache 前向）与全量前向数值等价 | llm.rs:574 起的移位 / argmax 双断言 |
| INV-8 | 端口号与层输出顺序一一对应且全图稳定：`outgoing_ports` 的排序去重 + BaseRunner 的 izip 共同保证 `outputs[i] ↔ port_i ↔ storage_key` | graph.rs:882、mod.rs:209-217、mod.rs:1107-1112 |
| INV-9 | claim 反向传播的正确性依赖反拓扑序：下游 claim 必须先于上游节点被登记（`backward_iter` 顺序保证） | prover.rs:717-853、claims 登记以 `NodeInput(node_id, i)` 为键（prover.rs:840-846） |
| INV-10 | 跨 chunk 一致性由"公共挑战点 + 承诺 id 绑定 + transcript 绑定"三件套保证，无需 chunk 间通信 | prover.rs:697-705、chunking.rs:696-712、prover.rs:600 |
| INV-11 | tracked_layer_data 仅在量化校准运行中被消费（TrackerRunner），正常推理不读取、不持久化 | mod.rs:710、provable/mod.rs:213 |

---

## 7 已知限制与权衡

| 编号 | 限制 / 权衡 | 说明 | 影响评估 |
| --- | --- | --- | --- |
| L-1 | dry 推迟显存释放 | 后台线程持有克隆引用，设备内存释放延后到数据消费完成 | 显存峰值可能高于理论活跃集；长上下文时需关注 |
| L-2 | 每节点固定调度开销 | 每层调用穿行四层 Runner：查表组装输入、克隆 handle 发通道 | 与算子计算量相比目前可忽略；若算子计算被大幅优化（如多核并行化 GEMM），此开销将成为新的可测量项 |
| L-3 | 结果通道容量固定为 10 | `sync_channel(10)` 的背压在 token 生成速度极快时可能短暂阻塞调度 | 影响有限；如需更高吞吐可评估调整容量 |
| L-4 | trace 记账与数据解耦的双面性 | Trace 仅含引用，跨进程消费必须依赖 store 可达或单文件内嵌 | 决定了持久化方案的设计空间（单文件 vs store 重建） |
| L-5 | LLM 计数刷新依赖 `model_inputs` 被按时调用 | 若新增 Runner 在多轮场景下绕过该钩子，引用计数将失衡 | 扩展时的强制约定 |
| L-6 | chunk 切分受 Add 节点约束：残差汇合点必须整块落在同一 chunk，attention/einsum 不能跨 chunk 任意切 | LLMChunkingStrategy 的设计权衡（chunking.rs:1704-1709） | chunk 数上限受制于 Add 节点分布；模型结构变化时需复核策略 |
| L-7 | prove 阶段要求 chunk 内每个节点的 trace step 必须存在且可水合 | `get_step` 失败即 bail（prover.rs:639-643、720-722） | trace 与 chunk 结构必须同步生成；手工裁剪 trace 会导致证明失败 |

---

## 附录 A 代码索引

| 机制 | 位置 |
| --- | --- |
| `LayerRunner` trait（`model_inputs` / `run_layer`） | mod.rs:118-145 |
| `RunInput` 定义 | mod.rs:125-131 |
| `BaseRunner`（懒加载、输出打包、proving handle、split 映射） | mod.rs:147-246 |
| `StoreRunnerMsg` 通道消息类型 | mod.rs:454-479 |
| `StoreRunner` 构造 + 后台线程 | mod.rs:481-540 |
| `StoreRunner::send` / `send_element` | mod.rs:542-562 |
| `StoreRunner` 发送规则（outputs / chunks / recombination / proving） | mod.rs:600-673 |
| `StoreRunner::Drop`（排空 + join） | mod.rs:640-656 |
| `TraceRunner` 构造 / run_layer / build_trace_outputs | mod.rs:796-849 / 855-889 |
| `HandleLifetimeRunner` 构造（输出预加引用） | mod.rs:902-958 |
| `HandleLifetimeRunner::model_inputs`（计数刷新点） | mod.rs:1002-1044 |
| `HandleLifetimeRunner::run_layer`（查表供输入 + dry 回收） | mod.rs:1046-1096 |
| `run_with_runner`（拓扑驱动循环） | mod.rs:1552-1585 |
| `run_with_split_nodes_info`（栈组装） | mod.rs:1587-1625 |
| `run_float`（纯推理双层栈对照） | mod.rs:1628-1643 |
| token 循环 | llm.rs:495-547 |
| 结果线程（EOS / token 收集） | llm.rs:469-494 |
| heavy run 与一致性断言 | llm.rs:549-625 |
| `Trace` / `Step` 结构定义 | trace.rs:29 / 408 |
| `Trace::new_steps_for_splitted_nodes` / `new_step` | trace.rs:150 / 294 |
| handle 懒加载 `load()` / dry | handle.rs:375-410 / 451 |
| concatenation cache（层内增量 KV） | transformer/mod.rs:19-117 |
| `GenStore::fetch` / `store`（后台线程最终调用） | genstore.rs:211-243 |
| `Evaluate` trait / `LayerOut` / `ProvingData` / `ProvingHandle` / `TrackedDataId` | provable/mod.rs:308 / 207 / 44-60 / 66-71 / 204 |
| Activation 的 track 登记 / tracked id 生成 | activation.rs:434-466 / 70-78 |
| Logits evaluate（ArgmaxData 产出） | logits.rs:382-394 |
| Softmax evaluate（SoftmaxData）/ prove / lookup witness | softmax/evaluate.rs、prove.rs:11、lookup.rs:124 |
| LayerNorm 量化评估（mean/std_dev） | layernorm/evaluate.rs:91 |
| TrackerRunner（tracked_data 唯一消费者） | mod.rs:669-713（drain：710） |
| `track_intermediate_data` / global_data_trackers | llm_tracker.rs:159-170 / 26-27 |
| `NodeId` / `NodeOutput` / `NodeInput` / `output_at` | graph.rs:213-235 |
| `incoming_feeds` / `outgoing_feeds` | graph.rs:518 / 538 |
| `outgoing_ports`（排序去重） | graph.rs:882 |
| `to_storage_key`（NodeOutput / NodeId） | mod.rs:1107-1112 |
| `ChunkingStrategy` trait / Default / LLM 策略 | chunking.rs:969-986 / 1000 / 1710 |
| `chunk_trace`（全量 trace 抽子 trace） | chunking.rs:553 |
| boundary 承诺一致性校验 / 边界 claims | chunking.rs:696-712 / 717-746 |
| `chunked_prove_local`（证明入口） | prover.rs:1043 |
| 证明执行图（SplitNode / ChunkProver / Final） | prover.rs:939-1020、prover_graph.rs:158-210 |
| `prove_chunk`（九阶段主流程） | prover.rs:588-918 |
| `instantiate_witness_ctx_for_chunk`（witness 批量承诺打点） | prover.rs:1171-1210 |
| claim 反拓扑主循环 / 播种与登记 | prover.rs:717-853（登记：840-846，播种：848-853） |
| `generate_lookup_witness_for_chunk` | lookup/context.rs:224 |
| `prove_lookup_op`（查表协议骨架） | lookup/operation/generic_prove.rs:66 |
| einsum sumcheck | einsum/prove.rs:41 起 |
| `to_verifier_io` / `element_volume` | trace.rs:311 / 343 |
| LLM verifier（EOS / 移位校验） | llm.rs:830-890 |
