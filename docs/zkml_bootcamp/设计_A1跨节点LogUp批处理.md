# 设计 · A1 跨节点 LogUp 批处理（M1/D1.2）

> 目标：把 `prove_chunk` 内每个查表节点各自独立的一次 LogUp GKR 证明
> （`prove_lookup_op` → `new_batch_multiple_sizes_prove`，gemma-270m 48 段 × ~9 节点）
> 合并为**每 chunk 一次**批量 GKR。实测依据：查表族占证明算力 82%（N 文档 §1.3），
> 其中逐节点 GKR 的固定开销（transcript 轮次、实例批处理、claim 提取）预计压减
> 查表族 10-20%。

## 1 现状流程（per node，`generic_prove.rs::prove_lookup_op`）

```
prove_node(节点):
  ① 主 sumcheck（IO claim 链）→ last_claim（本节点输出 claim）
  ② prover.lookup_witness(node_id) → 已承诺 witness 多项式（trace 期生成）
  ③ create_logup_inputs(...)        ← 本节点全部表实例
  ④ new_batch_multiple_sizes_prove(&logup_inputs, transcript)   ← 每节点一次 GKR
  ⑤ prove_linking_sumcheck(mles, transcript, last_claim, logup_proof.point(), ...)
```

verifier 对称：`verify_node` → 主 sumcheck 验证 → `verify_lookup_op`（logUp 验证 +
linking 验证）。

## 2 目标流程（per chunk 两遍）

```
第一遍（节点循环，与现在一致）:
  ① 主 sumcheck → last_claim
  ② witness 多项式 → create_logup_inputs → 暂存 LogupDeferred { inputs, node_id }
  （③⑤ 延后）

chunk 级:
  ③ new_batch_multiple_sizes_prove(所有节点的全部实例, transcript)  ← 每chunk一次
     → per-instance output_claims（复用现有 logup_batch_proof.output_claims() API，
       prove_tables 已按相同方式按表切片提取）

第二遍（节点循环，逆序或原序均可——transcript 吸收顺序两侧一致即可）:
  ④ 从批量 proof 提取本节点实例的 claims → LogupBatchVerifierClaim
  ⑤ prove_linking_sumcheck(...)（其余参数与现状完全一致）
```

## 3 改动清单（文件级）

| 文件 | 改动 |
| --- | --- |
| `zkml/src/lookup/operation/generic_prove.rs` | `prove_lookup_op` 拆为 `build_logup_inputs`（①②）与 `prove_linking_only`（⑤，接收批量 proof 的本节点 claims）；新增 `finalize_chunk_logup(deferred[], transcript)`（③④） |
| `zkml/src/lookup/operation/generic_verify.rs` | `verify_lookup_op` 同拆：`collect_logup_expectation`（吸收输入到 transcript，不验证）与 `verify_linking_only`；新增 chunk 级 `verify_batched_logup` |
| `zkml/src/iop/prover.rs` | `prove_chunk`：节点循环中暂存 deferred；循环后调用 finalize；`LayerProof` 组装时嵌入本节点 claims 引用 |
| `zkml/src/iop/verifier.rs` | `verify_chunk` 节点循环同拆：第一遍只验主 sumcheck 并吸收 logup 输入；循环后验批量 GKR；第二遍逐节点 linking |
| `zkml/src/layers/*`（LayerProof 各变体） | 查表层 proof 结构中 `logup_proof` 字段从 per-node 移到 chunk 级 `ChunkProof.batched_logup`；各变体保留 linking/claims 字段 |

## 4 不变量与风险

1. **transcript 顺序**：新顺序 = [全部节点主 sumcheck + logup 输入吸收] → 批量 GKR
   → [逐节点 linking]。**两侧必须逐字节一致**——这是唯一硬约束；
2. **per-instance claims 提取**：`output_claims()` 的实例顺序 = 输入顺序（现有
   `prove_tables` 已依赖此序），批量化后实例数 = Σ节点实例数，切片按节点实例数
   前缀和分派；
3. **ChallengeStorage** 不变（chunk 级、表级挑战，先于一切节点采样）；
4. **P1 交互**：linking sumcheck 的 claim 来源从"本节点 GKR"变为"批量 GKR 切片"——
   若批量 GKR 有缺陷则**同 chunk 全部节点**失败（失败模式更集中，猎捕 dump 直接
   定位到批量 proof）；
5. **协议安全**：GKR 实例合并用随机系数（吸收全部实例承诺后采样）——与
   `new_batch_multiple_sizes_prove` 现有多实例合并语义相同，无新增假设。

## 5 验收

1. gemma-270m s=512 端到端 verify 通过（A4+S1 基线上）；
2. 段证明时间对照：同配置 -10% 以上（layer_requant 层计时对照）；
3. S1+P1 猎捕循环在新结构下继续采样（失败率不回升）。

## 6 工程量

~400 行跨 5 文件；两遍化重构的 verifier 镜像为主要工作量。建议独立分支/独立
commit 序列（proof 结构变更多，逐层可回退）。
