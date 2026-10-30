# 流式 Trace 分段证明机制

## 文档控制

| 项目 | 内容 |
| --- | --- |
| 文档编号 | J |
| 所属系列 | zkml_bootcamp |
| 版本 | 1.0 |
| 发布日期 | 2026-10-30 |
| 文档状态 | 已发布 |
| 适用代码基线 | `zkml/src/iop/streaming.rs`、`zkml/src/iop/chunking.rs`、`zkml/src/model/mod.rs`（`run_and_prove_streaming`）、`zkml/src/model/llm.rs`（`Driver::prove_streaming` / `verify_streaming`）、`zkml/src/iop/verifier.rs`、`tenstore/src/metrics.rs` |

### 修订记录

| 版本 | 日期 | 修订说明 |
| --- | --- | --- |
| 1.0 | 2026-10-30 | 首次发布：预算驱动切分、段完成检测与内存 pin、双执行模式（同步/流水线）、方案 A 种子链、FPGA/ASIC 对接接口契约、验收数据 |

---

## 1 引言

### 1.1 目的

本文档规定流式分段证明机制的详细设计：trace 不再一次性完整生成后再证明，而是按内存预算切成段（segment），生成完一段即在内存中证明一段并释放，全部段证明完后合并为一个最终 `Proof`。作为流式证明路径的维护、扩展（水平切分、长上下文）与 FPGA 加速对接的权威参照。

### 1.2 范围

覆盖：切分策略、段完成检测、内存 pin、双执行模式、种子链派生、证明合并与验证、对外接口契约。不覆盖：协议安全性论证（见 K 文档）、Runner 洋葱栈基础机制（见 I 文档）、PCS 数学构造。

### 1.3 与经典路径的对照

| 维度 | 经典路径（I 文档） | 流式路径（本文档） |
| --- | --- | --- |
| trace 生命周期 | heavy run 一次生成全量 → 全量证明 | 分段生成，段证明后即释放 |
| 磁盘依赖 | prove 阶段按需从 store 重载（LRU 未命中读盘） | prove 全程 `disk_reads = 0`（pin 保证内存命中；store 写盘仅兜底） |
| 峰值内存 | 全量 trace + 证明工作集 | 窗口内段 × 预算 + 单段证明工作集 |
| 挑战派生 | 全局单挑战，各 chunk 共享 | 方案 A：串行种子链（K 文档 §5），验证入口 `verify_streaming` |
| 验证器 | `verify` | `verify_streaming`（业务校验共享） |
| 切分粒度 | 按模型结构（Add 节点 / 水平切分） | 内存预算驱动（一期仅垂直切分） |

---

## 2 总体设计

```text
图 1  流式分段证明数据流

输入(完整生成句子)
  │
  ├─ 预切分: LLMChunkingStrategy::new_vertical_only + num_chunks_for_budget
  │          → Vec<ModelChunk>（只依赖图结构+输入形状, 无需 trace）
  │
  │   拓扑序逐节点 (Model::run_and_prove_streaming)
  │   ├─ 进入段 c 前: pin(段c 的全部 storage_key)         ← HandleLifetimeRunner
  │   ├─ BaseRunner/StoreRunner/TraceRunner 照常执行       ← I 文档机制不变
  │   ├─ TraceRunner step tap → 段子 trace 逐步累积
  │   └─ 段 c 节点全部完成
  │       ├─ Sync:      专用大栈线程就地 prove → 吸收种子链 → 释放 pin
  │       └─ Pipeline:  投递有界通道(容量 W) → 常驻证明线程 prove
  │                     → 吸收种子链 → 通知 run 循环释放 pin
  │
  └─ finish: 等待全部段 → Vec<ChunkProof>(完成序) → Proof
             trace.input/output → to_verifier_io → IO
```

**核心不变式**：任何张量在被其所属段的证明读取之前，usage 计数或 pin 计数至少一个大于 0（不会中途被 dry 丢内存副本）；证明完成后两者皆 0 时立即 dry 释放。

---

## 3 组件详细设计

### 3.1 预算驱动切分器（chunking.rs）

- `LLMChunkingStrategy::new_vertical_only(input_length, model)`：构造带 `disable_horizontal_split` 标志的策略，einsum/logits 不做水平切分（流式一期的子 trace 提取不支持 `SplittedNodes`）。
- `node_trace_volumes(model)`：从 `unpadded_shapes`（切分器构造时已解析计算）得到每节点输出元素数 × 查表类算子系数（Softmax/LayerNorm/RMSNorm/Logits ×2，估算 proving data 体积）。
- `num_chunks_for_budget(budget_bytes, model)`：排除保留段（logits+投影、embedding）后，`段数 = ceil(流体积 / 预算) + 2`。单节点体积超预算时 `warn!` 告警放行（一期行为；二期水平切分消除）。
- 切分执行复用 `ProverContext::split_in_chunks(Some(n), strategy)`——与经典路径完全相同的 ChunkedGraph/边界机制。

### 3.2 段完成检测与子 trace 提取（streaming.rs）

- `TraceRunner` 增加可选 step tap：每记录一个 `Step` 克隆发送（无界 mpsc，send 失败不阻塞运行）。
- `StreamingProver` 维护：每段未完成节点数（`remaining`）、节点→段映射、段的 storage_key 集合（`chunk_keys`：节点入边 source key ∪ 出边 port key，全部可在运行前从图确定）。
- 段完成时用累积的 steps 组装子 trace（`Trace { steps, input: [], output: [] }`）——与经典 `chunk_trace` 等价，Step 内 handle 为 Arc 克隆。

### 3.3 内存 pin（model/mod.rs `HandleLifetimeRunner`）

- `pin_counts: HashMap<StorageKey, usize>` 与 `handle_usage_count` **相互独立**；dry 条件由"usage==0"改为"usage==0 且未 pin"（三个 dry 点全部接入：入边消耗后、未用输出、未用输入）。
- pin 时机（run 循环）：进入段 c（首个节点执行前）`pin_keys(chunk_keys[c])`；释放：段证明完成后 `release_pins`（usage 与 pin 皆 0 → 立即 dry）。
- 跨段张量（段 c 输出 = 段 c+1 输入）：同时出现在两段的 key 集合中，相邻两次 pin/释放之间由 usage 计数桥接，全程不丢。
- 模型输入整场永久 pin（喂首段 + 结尾 `to_verifier_io` 需要 wet）。
- proving data（softmax shift / layernorm mean 等）无 usage 计数、从不被 dry，天然全程内存命中，无需 pin。

### 3.4 双执行模式（StreamingMode）

| 模式 | 行为 | 适用 |
| --- | --- | --- |
| `Sync` | run 循环在段完成点阻塞，spawn 专用大栈线程（64MB）就地证明，join 后释放 pin | 最小内存形态；调试基线 |
| `Pipeline { window }` | 常驻证明线程（64MB 栈）从有界通道（容量 window）FIFO 取段证明；run 循环投递后继续生成；通道满即背压阻塞（持 pin 等待，内存有界） | 性能形态；总时间 ≈ max(生成, 证明) |

两模式共用：切分/pin/子 trace/种子链/合并。`Pipeline` 模式下段证明完成经 `release_tx → take_released()` 通道通知 run 循环释放 pin。

### 3.5 种子链（方案 A，K 文档 §5.5）

`SeedChain`（streaming.rs）持有全局 transcript `S`（`b"model_stream_proving"` 域分离）：

1. 初始化：absorb(setup) ‖ absorb(输入绑定) ‖ absorb(段数) → `seed₀`；
2. 段 i 证明用 `initialize_transcript_for_chunk(seed_i)`；
3. 段 i 证明完成后 absorb(段结构 + IO 承诺) → `seed_{i+1}`。

**吸收顺序 = 段完成顺序 = 最终 `Proof.chunk_proofs` Vec 顺序**；验证器 `verify_streaming` 按该 Vec 顺序重放。Sync 模式下 SeedChain 由协调器持有；Pipeline 模式下随段任务队列**迁移到证明线程**（FIFO 保证顺序）。

### 3.6 证明线程

专用线程 `PROVE_THREAD_STACK_SIZE = 64MB`：
- sumcheck/expression 求值与大型结构递归 drop 需要的栈深超过小栈线程（如 libtest 2MB）所能提供；显式大栈使证明与调用方线程栈无关。
- 该线程即 FPGA 桥接的软件参考形态（§5）。

---

## 4 关键流程

### 4.1 端到端（`Driver::prove_streaming`）

```text
prove_streaming(ctx, input_tensor(完整生成句子), budget, mode, store)
  ├─ 垂直切分: new_vertical_only + num_chunks_for_budget + split_in_chunks
  ├─ Model::run_and_prove_streaming   ← 整场包在 thread::scope 内
  │    ├─ Runner 栈组装（同 I 文档, StoreRunner 照常兜底写盘）
  │    ├─ pin(模型输入) + 逐节点: pin(段) → run_layer → 段完成 → 派发证明 → 释放
  │    └─ finish: 等待证明线程 → Vec<ChunkProof> → Proof; to_verifier_io
  └─ 返回 (Trace, Proof, IO)
```

**输入语义**：`input_tensor` 必须是**完整生成句子**（与 `run_elements` 的 heavy run 相同输入）——LLM 验证器的移位校验要求贪心预测与真实下一 token 一致。当前 `bench --streaming` 先跑一次 `run_elements` 参考生成句子（heavy run 双跑，见 §7 L-2）。

### 4.2 验证（`LLMVerifierContext::verify_streaming`）

与经典 `verify` 共享全部业务校验（EOS 位置、因果移位、输入一致性），仅密码学重放路径不同：`Verifier::verify_streaming` 重放种子链（K 文档 §5.5）。流式证明**必须**用流式入口；负向测试固化两派生互斥。

---

## 5 FPGA/ASIC 对接接口契约

流式路径把证明端隔离在一个串行会话内，加速器只需实现"段进→证明出"。对接形态（软件参考实现即 `pipeline_prover_thread`）：

| 阶段 | 方向 | 载荷 | 说明 |
| --- | --- | --- | --- |
| 会话建立 | 主机→加速器 | setup 摘要（PCS 参数标识）、seed₀ | 加速器据此初始化段 transcript |
| 段投递 | 主机→加速器 | `(chunk_id, chunk 结构[可序列化 ModelChunk], 段张量数据, seed_i)` | `ModelChunk`、`Trace`、`ChunkProverInput` 均 `Serialize`；张量数据 = 段内全部 handle 的（storage_key, shape, 元素数组） |
| 段证明 | 加速器内 | — | 与软件 `prove_chunk` 协议一致（阶段 2-9，I 文档 §5.2） |
| 段回传 | 加速器→主机 | 序列化 `ChunkProof` | 主机随即：①吸收种子链派生 seed_{i+1}（`SeedChain::absorb`，主机侧函数）；②通知释放该段内存 |
| 会话收尾 | 主机→加速器 | 结束标记 | 全部段完成后合并 `Vec<ChunkProof> → Proof`（完成序） |

**顺序约束**：段必须按**完成顺序**串行投递（seed 链依赖前段证明产物）；段内 sumcheck 的 rayon 并行不受影响。**信任边界**：加速器完全不受信——其产出必须通过 `verify_streaming`；验收即"加速器证明被软件验证器接受"。

**内存契约**：主机侧每段投递后即可在窗口内继续生成；段内存（pin）在收到该段回传后释放。加速器侧每段证明工作集 ≈ 该段 Fr 多项式。

---

## 6 设计约束与不变式

| 编号 | 不变式 | 依据 |
| --- | --- | --- |
| INV-S1 | pin 独立于 usage 计数；dry 条件 = usage==0 且 pin==0 | HandleLifetimeRunner 三个 dry 点统一检查 |
| INV-S2 | 段的 key 集合（pin 集合）在运行前即可从图结构完全确定 | 节点入边 source ∪ 出边 port 的 storage_key；proving data 永不 dry |
| INV-S3 | 段吸收顺序 = 完成顺序 = `Proof.chunk_proofs` Vec 顺序 | 种子链正确性（方案 A）；verifier 按 Vec 序重放 |
| INV-S4 | 段证明在专用大栈线程执行 | 栈深需求；FPGA 桥接的软件参考形态 |
| INV-S5 | 流水线窗口 W 有界背压；通道满时 run 循环持 pin 阻塞 | 内存峰值 ≈ W×预算 + 单段证明工作集 |
| INV-S6 | 流式证明必须用 `verify_streaming` 验证 | 两派生互斥；负向测试固化 |
| INV-S7 | 模型输入整场永久 pin | 首段消费 + 结尾 verifier IO 零 fetch |
| INV-S8 | prove 期间 `store.disk_reads == 0` | pin 全覆盖；测试断言固化（store 流量计数器） |

---

## 7 已知限制与权衡

| 编号 | 限制 / 权衡 | 说明 |
| --- | --- | --- |
| L-1 | 一期仅垂直切分 | 单节点超预算告警放行；水平切分（二期）消除 |
| L-2 | 生成跑两遍 | 当前 `prove_streaming` 输入需完整句子，bench 先跑 `run_elements` 参考 pass。生产化改造：`run_elements_with_runner` 暴露"仅 token 循环、跳过 heavy run"模式，heavy run 由流式证明融合执行 |
| L-3 | 单证明线程（段间串行） | 方案 A 种子链要求段序派生；段内 sumcheck 并行不受影响。段间并行需演进为 Merkle root 派生（K 文档 §5.3），FPGA 多核加速器在段内并行 |
| L-4 | 表证明每段一次 | 段数增多时查表证明固定开销线性增长；候选优化：表证明全局化（每表一次），见 K 文档 §6 |
| L-5 | prove 递归的栈深根因未除 | 大段在 2MB 栈溢出，已用 64MB 专用线程规避；根因（疑似递归 Drop）待查（阶段 5） |
| L-6 | 32k/长上下文需 SWA | 全注意力 s² 项不可行（F 文档）；滑窗注意力改造是 Llama-7B/16k 的前置条件（阶段 6 spike） |

---

## 8 验收数据（GPT-2 124M, safetensors, ctx=10 tokens, 128 核 CPU, BN254/HyperKZG）

`test_llm_driver_streaming_prove_gpt2`（3 段：embedding 2 节点 / 主干 255 节点 / logits 2 节点）：

| 指标 | 结果 |
| --- | --- |
| `verify_streaming`（含移位校验） | 通过 |
| 经典 `verify` 对流式证明 | 失败（负向断言，两派生互斥） |
| prove 期间 store 磁盘读 | `disk_reads = 0, fetch = 0` |
| 兜底写盘 | 347 页，~10.3 MB（后台线程） |
| 流式输出 vs `run_elements` | 逐 token 一致 |
| Pipeline{window:2} 模式 | 通过；生成与证明重叠 |

ctx=128 基准（`bench-llm --streaming`，512MB/256MB 预算）数据待补（模型缓存环境就绪后运行，CSV 列：`streaming_prove_full`、`store_*`、`prove_full_memory_peak`）。

---

## 附录 A 代码索引

| 机制 | 位置 |
| --- | --- |
| 预算切分（垂直模式/体积/段数） | `chunking.rs`：`LLMChunkingStrategy::{new_vertical_only, node_trace_volumes, num_chunks_for_budget, disable_horizontal_split}` |
| 协调器（计划/检测/派发/合并） | `iop/streaming.rs`：`StreamingProver` |
| 种子链 | `iop/streaming.rs`：`SeedChain` / `build_seed_chain`；verifier 侧 `Verifier::verify_streaming` |
| 绑定函数（W1/W2/W3） | `iop/verifier.rs`：`bind_io_inputs_to_transcript` / `bind_proven_chunk_to_transcript` / `bind_chunk_count_to_transcript` |
| 内存 pin | `model/mod.rs`：`HandleLifetimeRunner::{pin_keys, release_pins, is_pinned}`；dry 点三处 |
| step tap | `model/mod.rs`：`TraceRunner::{with_step_tap, step_tap}` |
| 编排（scope/双模式/释放） | `model/mod.rs`：`Model::run_and_prove_streaming` |
| LLM 入口/验证 | `model/llm.rs`：`Driver::prove_streaming`、`LLMVerifierContext::{verify, verify_streaming, verify_inner}` |
| 专用证明线程 | `iop/streaming.rs`：`PROVE_THREAD_STACK_SIZE`、`pipeline_prover_thread`、`prove_task` |
| store 流量计数器 | `tenstore/src/metrics.rs`、`GenStore::{metrics, metrics_snapshot, reset_metrics}` |
| bench 集成 | `zkml/src/bin/bench/llm.rs`：`--streaming --stream-budget-mb --stream-window` |
| 验收测试 | `zkml/src/model/llm.rs`：`test_llm_driver_streaming_prove_gpt2` |
