# L06 · zkML 全景与 DeepProve 系统精讲

> 学习目标：建立 zkML 的问题地图（谁在证明什么、难点在哪）；掌握量化推理与证明的接口；完整掌握 DeepProve 的架构与两类算子协议；能端到端讲清一次证明。
> 前置：L02~L05。预计 2 周（含系统主教材精读）。

---

## 1. zkML 的问题地图

**要证什么**：给定公开模型 M 与输入 x，证明"y = M(x) 这条推理确实按 M 的权重与算子语义算出"。验证方（用户/审计方/链上合约）只拿到 (M 的承诺, x, y, π)，毫秒级核验。

**为什么难（三大难点，决定架构）**：

1. **规模（用 GPT-2 124M 算实账，s=512）**：
   - 前向 MAC 数 ≈ 2·参数量·s = 2×1.244×10⁸×512 ≈ **1.27×10¹¹**——证明系统若逐门电路化完全不可行，必须"线性证明 + 常数/对数验证"；
   - 权重域化后 1.244×10⁸×32B ≈ **3.97 GB**（setup 期一次承诺，静态常驻）；
   - trace（i64，8B/元素）可逐项核算：**score 项精确可算** = 12 层×12 头×512²×8B = 144×262,144×8 ≈ **302 MB**；其余每层中间量（qkv/attention-out/ln/fc/gelu/proj 等，张量数依图而定）≈ 30–40 MB/层，12 层 ≈ **0.4 GB** 量级；
   - logits 全量 = 512×50257×8B ≈ 206 MB，但 **final-token-only 只证明最后一行**（1×50257×8B ≈ 0.4 MB）——这是"证明开销 O(s²)→O(s)"的定量含义；
2. **非线性**：激活/归一化/量化不是多项式（L05 已解）；
3. **量化算术**：真实部署用 int8/fixed-point，证明系统活在素数域——必须证明"整数计算在域中的像"，含溢出/移位/舍入的语义绑定。

**主要系统对照**（按标题检索论文）：zkCNN（CNN，sumcheck+GKR 派，FFT 委托的出处）、EZKL（ONNX 通用、电路式）、zkml（Kang 等，dissupported op 级定制）、DeepProve（本文，layer-wise sumcheck + LogUp + HyperKZG）。对比维度：算子覆盖、证明开销 vs 推理开销比值、验证成本、PCS 路线、是否需要 trusted setup。

## 2. 量化推理与证明的接口

DeepProve 采用**仿射量化 + 定点算术**（论文 §2.1）：实数张量 x ≈ scale·(x_q − zero_point)，矩阵乘在 int 累加域完成，requant 层用"定点乘子 + 右移"回归量化格点。证明视角：

- 推理全程在 **i64** 上运行，中间量写入 **Trace**（每个节点的输入/输出快照 + 辅助量如 LN 的 mean/std、softmax shift）；
- 证明把每个 i64 **域化**（负数 → p−|x|）后，验证"域中等于" ⟺ "整数计算正确"（只要 i64 不溢出——位宽预算由算子保证，如卷积 output_bitsize 公式）；
- requant 的乘子/右移不是域运算，而是**查表**（L05）——整数语义与域语义在此对齐。

**Trace 是唯一的数据通道**（`model/trace.rs`）：`Trace{steps: HashMap<NodeId, Step>}`，`Step{node_inputs, node_outputs, proving_data}`。流式证明模式下 trace 逐步产生、逐步消费（`StreamingProver`，详见系统主教材 §一/§三）。

## 3. DeepProve 架构：PIOP + PCS 的分层

```
┌─ PIOP 层（证明"算得对"）────────────────────────────┐
│ 线性层：einsum sumcheck（论文 Protocol 3/4）          │
│ 非线性层：LogUp-GKR + linking sumcheck（Protocol 5 等）│
│ 组合：layer-by-layer（GKR 范式，claim 链）             │
│ 批处理：batched sumcheck + RLC（所有 claim 归单点）     │
├─ PCS 层（证明"数据没换过"）──────────────────────────┤
│ HyperKZG：setup 承诺权重/表；每 chunk 承诺 witness     │
│ 一次三打开兑现全部 claim                               │
├─ 应用层 ────────────────────────────────────────────┤
│ final-token-only：只证最后一行 logits（O(s²)→O(s)）    │
│ chunk 切分：内存受限/分布式/流式                        │
└─────────────────────────────────────────────────────┘
```

**一次证明的端到端数据流**（背下来，这是"系统掌握"的验收线）：

```
前向推理(i64) → Trace → [按 chunk] → 域化 MLE → batch_commit(MSM)
  → 输出/边界 claim 播种（transcript 抽点）
  → 逆拓扑逐节点 prove（einsum sumcheck ∥ 查表 LogUp+linking）
  → claim 链闭合到 chunk 边界（承诺相等性 + common_point）
  → batched sumcheck → RLC → HyperKZG 三点打开
  → ChunkProof → seed 链合并 → Proof → Verifier 重放（毫秒级）
```

## 4. 线性层协议精讲（einsum）

**命题**（论文 Protocol 3）：`Σ_{k∈收缩维} LHS(行点,k)·RHS(k,列点) = 输出claim值`。三步构造（详细推导与 2×2 全数值例子见系统主教材 §5.5.3 与 FPGA 指导 OP-09，此处给骨架）：

1. **轴分类**：输出中出现 → Outer（固定）；双侧不求和 → Stacked（批量，每取值一张子表）；被求和 → Contracted（sumcheck 自由变量）；
2. **建表**：`mles_from_tensor` 从最后维向前扫描，Outer 维用 EQ 点积消去（读全张量：512·768 = 393,216 项 ≈ 12.6 MB/表）；
3. **sumcheck + 收 claim**：变量数 = ceil_log₂(收缩维)——GPT-2 的 d_model=768 → **n=10 轮**（2¹⁰=1024≥768）；终值三分类——活动输入（回传上游）、权重/偏置（对 setup 承诺的主张）、bias（直接求值）。

**Batched matmul（Protocol 4）**：stacking 挑战 + batching 挑战把多头/多输出并入一次 sumcheck。**卷积**：FFT 域按卷积定理证明，DFT 矩阵以 zkCNN 式委托链处理（FPGA 指导 OP-14）。

## 5. 非线性层协议精讲

L05 已给通用骨架；系统侧补三点：

- **辅助 witness**：LN/RMSNorm 的归一化因子、softmax 的 shift 都是推理期记录的 i64 辅助量，同样域化、承诺、进 linking sumcheck——"证明的输入是图 + trace 表，不是逐 token 状态"；
- **embedding**：查表（词 → 向量），LogUp 处理（论文 §4.3 的 chunks-of-table 技巧）；
- **logits**：只证最后一行（final-token-only），前面的行不进任何承诺——序列长度平方项被移出证明。

## 6. 验证侧：一次 Proof 的核对清单

`Verifier::verify_streaming`（verifier.rs:755-787）：

1. seed 链重放（setup 承诺 → 输入绑定 → chunk 数 → 逐 chunk 结构+IO 承诺吸收 → 派生下一 seed）——**验证方重放吸收序列 = 重放协议**；
2. 边界承诺相等（chunking.rs:622：下游入边承诺 == 上游出边承诺）；
3. 逐 chunk：重放各 sumcheck（每轮 O(d) 核对）、LogUp 两侧交叉、batched sumcheck 终点对承诺打开（HyperKZG verify：1 MSM + 1 配对）；
4. 首尾接模型 IO。

## 7. 自测题

1. 画出"稠密→GELU→稠密→logits"四节点模型的完整证明数据流（标注每张被承诺的表、每条 claim、每次 transcript 采样）。
2. 为什么 requant 可以只用一张表而 softmax 需要 shift？两者在 linking sumcheck 里的差别？
3. final-token-only 省掉了什么、保留了什么保证？如果用户想证明"全序列的 argmax 一致"需要改哪里？
4. chunk 间一致性的三个机制分别防什么作弊？
5. 为什么 witness 承诺在 chunk 内一次 batch 完成，而权重承诺是 setup 期全局一次？

## 8. 作业（P8）

- 画出题 1 的数据流图（一张 A4，标注代码行号）；
- 跑通 GPT-2 端到端：`zkml` 的 bench/llm 入口 → 产出 proof → `verify_streaming` 通过；记录证明耗时与 proof 大小；
- 精读系统主教材《prove_chunk_原理与实现详解.md》全文（本讲第二周的主任务），整理"你没有看懂的三处"提交导师。

## 9. 延伸阅读

- DeepProve 论文 §1-4（本讲对应）；§5 评测（两条规律：MSM 块数 ∝ 数据量、sumcheck 实例数 ∝ 节点数）；
- 系统主教材全文；FPGA 指导 §3-4（算子清单与调度序列——把"协议视角"换成"数据流视角"再读一遍）。
