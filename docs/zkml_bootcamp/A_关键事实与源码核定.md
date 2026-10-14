# A · DeepProve ZK 证明系统 —— 关键事实与源码核定记录

> **定位**：只回答「现状是不是如此 / 关键参数到底是多少」。不做评价、不给方案、不展开实现。
> **读者**：架构负责人、评审人，用于确认"传言/论文数字 vs 仓库现状"。
> **方法**：全部由关键源码锚点核实，三档标注——**已核实（源码逐行核过）/ 估算（由已核实常量推导）/ 论文数字（非本仓实测）**。以仓库根为基准；`dp-crypto` 锚点基于 commit `22e8e93`。

---

## 1. 核定范围与方法

- 只覆盖对"能否/如何上 FPGA"有影响的硬参数：域、承诺范式、分块粒度、各阶段数据量、打开负载。
- 判定规则：
  - **已核实**：能给出具体 `文件:行号` 且与 repo 快照一致。
  - **估算**：由已核实常量乘算得出（如 32 B/元素 × 张量元素数）。
  - **论文数字**：明确写"论文表 N，非本仓实测"，只作趋势参考。

---

## 2. 域与承诺范式（已核实，单一主线）

**结论：仓库只实例化一个范式——`ark_bn254::Fr`（254-bit，32 B/每域元素）+ `HyperKZG<Bn254>`（椭圆曲线 MSM）。**

- 域：`ark_bn254::Fr`，`Fp256`、4×u64 limb、全程 Montgomery。锚点 `lib.rs:306`、`number.rs:228`、`iop/mod.rs:225`、`poly_commit/mod.rs:220`、`bin/bench/llm.rs:34`。
- 承诺方案：`HyperKZG<Bn254>`；CUDA 下 `HyperKZGGpu<Bn254>`。锚点 `bin/bench/llm.rs:37`、`poly_commit/mod.rs:222`、`testing.rs:9`。
- 承诺负载 = **BN254 曲线 Pippenger MSM（多标量乘）**，不是"折叠码编码 + 哈希"。
- **仓库中不存在 BaseFold / Ceno / foldable code / Goldilocks 的活动实现。** `quantization/mod.rs` 仅在注释里把某些域列为 `ToField` 的"可能目标"；实例化恒为 `ark_bn254::Fr`。Goldilocks 64-bit 全文仅作理论对照。

**架构后果（一句话）**：若有 FPGA，承诺引擎要做的就是 **254-bit 模乘 + BN254 曲线 MSM**；这不是"64-bit 便宜 MAC"能覆盖的域。

---

## 3. 分块与关键参数（已核实）

| 参数 | 值 | 锚点 |
|---|---|---|
| 域元素宽 | **32 B**（`Fr`=32B、`G1Affine` 压缩 36B/非压缩 64B、承诺 32B） | `ark_bn254` 定义 |
| chunk 大小 | **2²⁰ = 1,048,576 元素** | `poly_commit/context.rs:103` |
| 单多项式最大 chunk 数 | **MAX_CHUNKS = 128** ⇒ 单多项式 ≤ 2²⁷ 元素 | `poly_commit/context.rs:112` |
| 缓存目录 | `model_cache/`（相对 CWD） | `parser` 加载路径 |

> 无网环境另一关键点（已核实，工程事实）：缓存目录相对启动目录，从仓库根跑与从 `zkml/` 跑读到**不同**缓存；离线环境须在同一 CWD 预置模型。

---

## 4. 各阶段数据量（估算，按 32B/元素；≤BN254 列为主）

| 阶段 | 产出 | GPT-2 (124M) | Gemma-3-270M |
|---|---|---|---|
| 预处理 | 模型域权重 | ~8 GB | ~17 GB（embedding 302M 元素 → 超 MAX_CHUNKS 被拆 4 多项式） |
| 推理 | 逐层 witness | ≤32 MB（seq≤512） | ≤64 MB |
| 承诺/sumcheck | 逐 chunk 承诺+轮消息 | 权重+表常驻、witness 流式 | 同左 |
| 打开 | 证明 bytes | 9–54 MB | 16–43 MB |

关键点：
- **权重 GPT-2 ≈ 8 GB、Gemma-3 ≈ 17 GB（均 BN254）**，远超单板 DDR、逼近中高端板 HBM（≥16GB）。
- **分块是唯一的 I/O 通吃粒度**：对应"读 32 MB、写 <1 KB"的纯读取带宽受限模式。

---

## 5. 打开负载修正（唯一一处"与论文不符"的已核实事实）★

论文表 7 给"打开 ≈ 1%"。**该数字在 HyperKZG 下不成立，需修正为 ≈ 承诺的 4×，且与承诺共享同一 MSM 引擎。**

依据（`dp-crypto` `arkyper/mod.rs`）：
- `commit`（fn 于 `mod.rs:457`，内 `msm::poly_msm` 于 `~461`）= **1 个 size=n Pippenger MSM**。
- `open`（`mod.rs:329-383`）= 折叠(Q(n)) + 中间承诺(Σ≈n) + **3 个 size=n** 的 witness MSM（via `kzg_open_batch@mod.rs:186`）→ G1 点乘 ≈ 4n ≈ **承诺的 4×**。
- `compute_witness_polynomial`（`mod.rs:171-183`）是串行 Horner 依赖链，对流水不友好。

**级联后果**：任何"承诺引擎"必须同时覆盖打开；验收口径不应只按"承诺 ~57%"计量，而应"承诺 + 打开"合并。占比数字（57%/35%/92%）为论文表 7，非本仓实测，仅作相对趋势。

---

## 6. 统一锚点表

| 事实 | 锚点 |
|---|---|
| 域 = `ark_bn254::Fr` | `lib.rs:306`、`number.rs:228`、`poly_commit/mod.rs:220`、`bin/bench/llm.rs:34` |
| PCS = `HyperKZG<Bn254>` / `HyperKZGGpu` | `poly_commit/mod.rs:222`、`bin/bench/llm.rs:37` |
| chunk=2²⁰ / MAX_CHUNKS=128 | `poly_commit/context.rs:103-112` |
| 整数→域 `to_field_mle` | `tensor/mod.rs:328` |
| 只证明末 token | `model/llm.rs:537` `last_token_tensor=...select(0,index)` |
| 承诺=1×Pippenger MSM | `dp-crypto arkyper/mod.rs:457`（`msm.rs:11 poly_msm`） |
| 打开四步（含 3×n MSM） | `dp-crypto arkyper/mod.rs:329-383`、`kzg_open_batch@mod.rs:186` |
| witness Horner 串行链 | `dp-crypto arkyper/mod.rs:171-183` |
| 批量 sumcheck EQ | `poly_commit/prover.rs:172/201` |
| 验证侧轻（1 小 MSM + pairing） | `dp-crypto arkyper` `kzg_verify_batch` |

---

## 7. 需上板/实测才能确认的项（本仓源码无既成数字）

1. **真实时间占比**（57%/35%）——论文数字，须在真实 BN254 域 benchmark 后定"先加速谁"。
2. **注意力 `[n,n]` score 矩阵是"整体物化承诺"还是"动态求值不物化"** —— 关系到二次项能否裁剪，源码无直接依据。断言见 §5 的"假设/待求证"性质。
3. **真实域算术吞吐基线**——约束特性（254bit vs 64bit）只有实测才闭环。

---

> 本文档只陈述事实与参数，不给方案。是否需要落地、如何落地，见 B（往哪投）与 C（怎么算）。
