# L03 · GKR 与分层证明

> 学习目标：理解 GKR 协议"电路层间归约"的范式；能说明 DeepProve 的 layer-by-layer 组合与经典 GKR 的同构与差异；能画出 claim 链并解释它为什么"既是归约也是批处理"。
> 前置：L02。预计 0.5 周。

---

## 1. 要证明的问题：电路算对了

给定算术电路 C（加法门 + 乘法门，扇出任意）、输入 x，P 声称 C(x) = y。直接对整个电路跑 sumcheck 不可行（电路不是超立方求和的自然形态）。**GKR（Goldwasser–Kalai–Rothblum, 2008）**的核心观察：

> 把电路按**层**（layer）切分：同一层的门都在同一"深度"。若已知"第 i+1 层的输出在某随机点的取值"，就能用一个 sumcheck 把它归约成"第 i 层输入在某随机点的取值"。

从输出层走到输入层，n_layers 次 sumcheck 之后，V 只需核对**真实输入**上的取值（公开数据，免费）。

## 2. GKR 层归约的构造（理解到能复述程度）

层的 wiring 由两个"选择谓词"刻画：add_i(a,b,c) = 1 ⟺ 第 i 层门 a 的输入 b 接到第 i+1 层门 c（加法门），mul_i 同理。层值函数记 W_i（定义在该层门编号的布尔超立方上）。第 i+1 层值恒等式：

```
W_{i+1}(z) = Σ_{a,b} add_i(a,b,z)·(W_i(a)+W_i(b))  +  Σ_{a,b} mul_i(a,b,z)·W_i(a)·W_i(b)
```

对随机点 z ← V 求值，两侧都是"关于 (a,b) 的超立方求和"，形状恰是 `Σ add(...)·W(a)·W(b)` 型的**三表乘积 sumcheck**（wiring 谓词先用 EQ 术随机化成表）。一次 sumcheck 把 W_{i+1}(z) 的 claim 归约成 W_i 在**两个新点**上的 claim（a、b 各一点），再对两个 claim 用随机线性组合合并。逐层往复。

- **V 复杂度（分层拆开说，避免含糊）**：每层 = ①一次 sumcheck 的核对 O(n·d)（n = log₂S 个变量、d = 层内门的度数上界）+ ②wiring 谓词在随机点的求值成本——这一项因电路而异：**一般电路需 O(S)**（V 得查 wiring 表），但**有代数规律结构的电路**（矩阵乘的行列结构、张量算子的规则布线）可用闭式在 O(polylog S) 内求值。GKR 的价值正是在后一类电路上：V 总成本 ≈ 各层 O(n·d) + 输入核对 O(S_in)，远小于电路规模；
- **P 复杂度**：每层 = 2·log S 轮 sumcheck，每轮折叠 S 级的表 → O(S·log S) 次域运算/层，总计 O(电路大小·深度)；

## 3. DeepProve 的 layer-by-layer 组合：与经典 GKR 的同与异

**同（范式同构）**：DeepProve 把每个算子层当作 GKR 的一"层"；claim 从输出端沿图逆拓扑传递；每次 `op.prove` = 一次层归约。论文原话：*"Similar to the fabled GKR protocol, we compose these operator protocols in a layer-by-layer fashion"*。

**异（三处工程化改造，要求能逐条讲清）**：

1. **层内协议按算子定制**：经典 GKR 的每层是统一的加/乘门求和；DeepProve 的线性层直接用 einsum sumcheck（跳过通用 wiring 表述），非线性层用查表论证 + linking sumcheck——比通用 GKR 快得多；
2. **claim 归约是"一条链"而非"两点半径"**：经典 GKR 每层产生 2 条 claim 需合并；DeepProve 的每个算子协议被设计成"输出 claim ⇒ 输入 claim"的**单链归约**（einsum 的 sumcheck 终点直接就是输入点），链上无需中途合并；
3. **wiring/输入的承诺与批处理**：权重是 setup 期承诺的静态表；多输入扇出用 same_poly/batched sumcheck 统一归拢（L02 §6），最终一次打开。

**claim 链的具体形态**（对照 `prove_chunk 详解` §5.5）：

```
输出 claim (transcript 抽点求值)
  → 逆拓扑逐节点：op.prove 吃"输出端口 claim"吐"输入端口 claim"
  → chunk 边界：出边 claim / 入边 claim 钉死
  → 步骤7 batched sumcheck + RLC → 一次 KZG 打开 → 输入端（公开模型输入）
```

## 4. LogUp-GKR：GKR 范式的另一处落地

查表论证不需要"电路层"，但 LogUp 把查表分数和编译成一个**天然的分层电路**（每层做分数加法），于是 GKR 的逐层 sumcheck 归约直接可用：每层一次 sumcheck，挑战点逐层传递（L05 详讲，电路表达式已在 FPGA 指导 OP-13 给全）。**这是"GKR = 逐层 sumcheck 归约范式"的最好第二例。**

卷积的 FFT 矩阵委托（FPGA 指导 OP-14）同理：DFT 矩阵的蝶形层级 × 每层一次 sumcheck。

## 5. 自测题

1. 用自己的话解释：为什么 GKR 每层归约后会产生**两个**新 claim（a、b 各一点），而 DeepProve 的 einsum 归约只有**一个**？（提示：einsum 的求和变量只出现在一张"已固定"的表上。）
2. 写出 DeepProve claim 链在"稠密层 + GELU"两个节点上的完整数据流（含 transcript 采样点）。
3. 经典 GKR 的 verifier 每层成本由哪两项组成？哪一项在 DeepProve 的 einsum 协议里被"闭式求值"消掉了？（提示：einsum 的 wiring = 求和索引，没有独立 wiring 表。）
4. 说明 batched sumcheck 与 GKR 层归约的关系：为什么说前者是后者的"静态批处理版"？

## 6. 作业（P4）

推导题（书面）：
1. 把 `Y = X·W`（X: 2×2）写成 GKR 单层电路的 W_{i+1}(z) 恒等式，并用 L02 例 1 的数值验证；
2. 给出"两个连续稠密层"的 claim 链图，标注每次 sumcheck 的被求和变量与挑战点；
3. 对比：若把整个两层网络编成一个 GKR 电路 vs DeepProve 的两个算子协议，P/V 各自的开销差异来自哪里？

## 7. 延伸阅读

- Thaler 书 §4.2（GKR 完整推导）；
- Goldwasser–Kalai–Rothblum "Delegating Computation: Interactive Proofs for Muggles"（STOC'08/JACM'15）；
- DeepProve 论文 §4（layer-by-layer 组合的官方表述）；
- 仓库：`prove_chunk 详解` §5.5（逆拓扑主循环逐行）、FPGA 指导 OP-09/OP-13/OP-14（三种"GKR 范式落地"的算子规格）。
