# DeepProve prove 流水线 · FPGA 算子开发指导

| 项目 | 内容 |
| --- | --- |
| 文档版本 | 1.1（v1.0 补全 OP-13 / OP-14 完整规格） |
| 日期 | 2026-11-04 |
| 适用代码 | 分支 `feat/streaming-trace-prove`（所有 `文件:行号` 以该分支为准；dp-crypto 指依赖 `Lagrange-Labs/dp-crypto`，源码位于 cargo git checkout） |
| 读者对象 | FPGA 开发工程师（不要求具备零知识证明背景） |
| 文档目标 | 作为 prove 流水线硬件化的**唯一需求来源**：给出每个待实现算子的功能定义、精确算法（伪代码级）、协议作用、数值例子（可直接做测试向量）、硬件实现建议与验收方法，无需再查阅其它资料 |
| 配套文档 | 协议级背景（prove_chunk 逐步阐释）见 `prove_chunk_原理与实现详解.md`；本文自包含，仅在需要更深的协议动机时交叉引用 |

---

## 目录

1. [范围与总览](#1-范围与总览)
2. [全局约定](#2-全局约定)
3. [算子规格书](#3-算子规格书)
4. [集成设计](#4-集成设计)
5. [验证计划](#5-验证计划)
6. [风险与开放问题](#6-风险与开放问题)
- [附录 A：transcript 吸收与标签总表](#附录-atranscript-吸收与标签总表)
- [附录 B：代码位置索引](#附录-b代码位置索引)
- [附录 C：术语表](#附录-c术语表)

---

## 1. 范围与总览

### 1.1 证明流水线全景

FPGA 加速的对象是 `Prover::prove_chunk`（`zkml/src/iop/prover.rs:588`）内部的全部计算。它对模型的一个分片（chunk）执行八个步骤：

```
步骤0  chunk 结构绑进 transcript                      —— 主机/控制，无算术
步骤1  witness 域化(i64→Fr) + 批量承诺(MSM)            —— OP-02, OP-12, OP-10
步骤2  抽查表挑战                                      —— OP-03
步骤3  播种输出/边界 claim（随机点求值）                —— OP-04, OP-05
步骤4  逆拓扑逐节点证明：einsum→sumcheck；查表层→LogUp  —— OP-09, OP-07, OP-13, OP-06
步骤5  边界 claim 登记（witness claim 注册）            —— 控制面
步骤6  表侧 LogUp 批量证明                             —— OP-12, OP-13
步骤7  全部 claim 归单点(sumcheck) + RLC + KZG 打开     —— OP-07, OP-08, OP-06, OP-10, OP-11
步骤8  组装 ChunkProof                                 —— 主机
```

流式协调器（`zkml/src/iop/streaming.rs:272-285`）当前在专用线程上调用 `prove_chunk`，其代码注释明确此线程是**未来 FPGA 桥的接入点**——即：主机完成 trace 收集与 chunk 判定后，把 chunk 的张量与上下文交给加速器，加速器执行步骤 1~7 并回传 ChunkProof 的全部字段。

### 1.2 算子清单总表

| 编号 | 算子 | 服务步骤 | 规模量级 | 优先级 |
| --- | --- | --- | --- | --- |
| OP-01 | Fr 域算术单元（乘/加/减/逆） | 全部 | 每周期 1 次乘为目标 | P0（地基） |
| OP-02 | i64→Fr 域化（含 Montgomery 化） | 步骤1/4 | 每 chunk 全部 witness 元素 | P0 |
| OP-03 | Blake3 transcript / 挑战采样 | 全部 | 高频小包；挑战链的串行关键路径 | P0 |
| OP-04 | EQ 表生成 `evals` | 步骤3/4/7 | 每 claim 一张 2^n 表 | P0 |
| OP-05 | MLE 求值 / EQ 加权点积 | 步骤3 | 每 claim 一次 | P0 |
| OP-06 | 表折叠 `fix_low`（相邻对） | 步骤4/7 每轮；KZG Phase1 | 每轮读全表写半表 | P0 |
| OP-07 | sumcheck 轮引擎（轮多项式 + 两阶段并行） | 步骤4/7 | **主算之一**（占比随模型/规模变化，以 `measure.rs` 实测为准） | P0 |
| OP-08 | RLC 随机线性组合 | 步骤7 | 读全部已承诺表一遍 | P1 |
| OP-09 | einsum 预处理（Outer 轴固定 / 切表 / pad） | 步骤4 | 每 einsum 节点一次，数据重排为主 | P1 |
| OP-10 | 批量 MSM（Pippenger，共享基点） | 步骤1/7(KZG Phase2) | 每 2^20 元素块一次 | **P0（主算）** |
| OP-11 | HyperKZG 打开（三 Phase 全链） | 步骤7 | 每 chunk 一次 | P1 |
| OP-12 | 查表 witness 生成（位切片 + 值计数 + multiplicity） | 步骤1/6 | 每查表层节点一次 | P2（可后置到 CPU） |
| OP-13 | LogUp GKR 轮引擎（分数电路） | 步骤4/6 | 每查表层 `log₂表长` 轮 | P2（可后置） |
| OP-14 | 卷积 FFT 矩阵委托（zkCNN 式） | 仅卷积路径 | LLM 流水线用不到 | P3（按需） |

**明确不做进 FPGA 的部分**（保持主机）：chunk 切分与图调度、claim 寄存器簿记（`HashMap<NodeInput, Claim>` 的维护）、trace 生命周期与内存 pin 管理、ChunkProof 序列化、验证侧全部内容（配对运算等）。理由：它们是控制流/低频逻辑，且协议正确性依赖与 Rust 代码逐字节一致的 transcript 重放，先用主机跑通、再逐个下沉。

### 1.3 建议的实施顺序

1. **P0 一期**：OP-01/02/03/04/05/06/07 → 能跑通"单节点 einsum sumcheck"并和 CPU 逐位对齐；
2. **P0 二期**：OP-10 → 能完成 witness 批量承诺（数据量最大、收益最直接的一段）；
3. **P1**：OP-08/09/11 → 打通整 chunk 的端到端 prove；
4. **P2/P3**：OP-12/13/14（查表层与卷积可暂留 CPU，通过总线把 witness 表送进 FPGA 的 MSM/sumcheck 引擎）。

---

## 2. 全局约定

### 2.1 数据表示与序列化

**域元素 Fr**（BN254 标量域，p ≈ 2^254）：

- 计算内部建议 **Montgomery 表示，4×64-bit limb**（与 arkworks 一致；常数 R、R² 及 p 的参数取 `ark-bn254::Fr` 的发布值）；
- **transcript/存储/接口上的字节格式是 canonical（非 Montgomery）小端 32 字节**——dp-crypto 的 `append_scalars` 用 arkworks `serialize_compressed`（`arkyper/transcript/mod.rs:17-26`），即先去 Montgomery 再 32B LE 输出。硬件任何写入 transcript 的值必须做 Montgomery→canonical 转换；
- 逆元仅在少数位置出现（multiplicity 计算的除法、shift 求值除法），用费马小定理 `a^(p-2)` 或扩展欧几里得，低吞吐即可。

**i64 → Fr 映射**（OP-02 的精确定义，来自 arkworks `F::from(i64)` 语义）：正数 x → x；负数 x → p − |x|（因为 |x| < 2^63 < p，无多次归约问题）。

**G1 点**：承诺是 BN254 G1 仿射点。写入 transcript 的格式（`append_points`，transcript/mod.rs:47-63）：x 的 32B canonical LE ‖ y 的 32B canonical LE；**无穷远点固定写 16 字节全零**（`ZERO_POINT_SERIALIZED`，transcript/mod.rs:11）。ChunkProof 内部传输建议直接用 64B 仿射坐标。

**表（MLE）**：2^n 个 Fr 的连续数组，n = 变量数。**索引位序约定（全流水线统一，必须逐位复刻）**：

> **表索引的第 b 个二进制位 ↔ 求值点 r 的第 b 个坐标**（即第一个坐标是最低位）。
> - sumcheck 第 i 轮的挑战 r_i 折叠索引位 i（`fix_low`：相邻对合并，dense.rs:454-473）；
> - `evals(r)` 的倍增法最终布局恰好满足该约定（eq.rs:48-70，见 OP-04 的推导）；
> - 张量行主序展平时最后一个维度占最低位（`into_mle`）。

三个容易踩坑的事实，硬件实现与测试向量生成时必须遵守：

1. `fix_low` 合并的是**相邻对** `(z[2i], z[2i+1])`，不是跨半区对（dense.rs:494-502 `bound_poly_var_bot_01_optimized`：`par_chunks_exact(2)`）；
2. `evals(r)` 从**点的最后一个坐标**开始倍增，每轮把旧表"上移一位"、新坐标放到位 0——最终布局等价于"位 b ↔ r[b]"（eq.rs:48-70，用 r=(5,4) 手工追踪两轮即可验证，见 OP-04 例子）；
3. `eval_as_univariate` 把表当作**一元多项式系数**（`u(X)=Σ z_i·X^i`）在标量点求值（dense.rs:765-773），与"超立方体求值"是两种不同语义，不要混用。

### 2.2 标准流接口建议

为统一各算子的 RTL 契约，建议定义三种流：

| 流 | 单元 | 伴随侧带 | 用途 |
| --- | --- | --- | --- |
| 表流 table_stream | 1×Fr（32B） | poly_id、num_vars、段号 | witness 表 / EQ 表 / 折叠中间表的输入输出 |
| 点流 point_stream | 1×Fr | 序号 | 挑战、claim 点、求值点 |
| 标量流 claim_stream | {poly_ref, point[], eval} | — | claim 寄存器与 sumcheck 产物 |

所有大数据算子（OP-04/05/06/07/08/10/11）都建议实现为"表流入 → 表流出"的流式核，内部用双缓冲乒乓（见 §4.3）。

### 2.3 transcript 等价性规范（最高优先级需求）

**这是整个硬件化的生死线**：协议的所有随机挑战都从 transcript 派生，硬件只要有一次吸收的字节、顺序、格式与 CPU 不一致，挑战就全错，证明直接作废。

CPU 侧 golden 实现：`dp_crypto/arkyper/transcript/mod.rs` + `blake3.rs`。精确语义：

1. **哈希器**：BLAKE3 `Hasher`，构造时 `update(DST)` 吸入域分隔标签（如 `b"model_chunk_proving"`，chunking.rs:2337）；
2. **吸收**：`append_bytes(bytes)` = `state.update(bytes)`。三种封装：
   - `append_scalars(&[Fr])`：每个元素转 canonical 32B LE 后 update；
   - `append_points(&[G1Affine])`：每点 x(32B)‖y(32B)；∞ 点写 16B 零；
   - `append_bytes`：原样；
3. **采样**：`challenge_scalar()` = `state.finalize_xof().fill(16 字节)` → `F::from_le_bytes_mod_order`（mod.rs:29-34）。`append_and_sample(label)` = 先 update(label) 再采样；
4. **实现现状（重要，测试向量必须复刻）**：blake3 1.8.3 的 `finalize_xof(&self)` 每次返回**从 XOF 流位置 0 开始**的读取器（blake3 src/lib.rs:1413-1419）。因此**中间没有插入任何吸收的连续 `challenge_scalar()` 返回同一个值**——`read_challenges(n)`（zkml lib.rs:181-183，连续 n 次采样）得到的 n 个挑战**全部相同**。典型后果：`compute_claim` 采样出的求值点是 `(c,c,…,c)` 型对角点、same_poly 的 k 个合并系数相同。这不是硬件要修的问题（验证方逐字节重放，协议自洽），但**硬件测试向量不要假设挑战互异**，一切以 CPU dump 为准；
5. **标签清单**：全流水线用到的 DST 与采样标签见附录 A。新增代码路径时先在附录 A 登记标签再写硬件。

**延迟要求**：sumcheck 每轮在"发轮多项式 → 得挑战 → 折叠表"之间存在硬依赖（见 OP-07），挑战采样必须低延迟。每轮吸收的数据量 = (max_degree+1)×32B + 标签 ≈ 数百字节，BLAKE3 单块压缩即可出 16B XOF——建议 Blake3 核与主数据通路并行常驻，采样延迟目标 < 1 μs。

### 2.4 验证方法学

分层等价，逐层验收（详见 §5）：

- **L1 算子级**：对每个算子，从 CPU 运行中 dump 输入/输出对（§5.1），硬件逐位比对；
- **L2 实例级**：单次 einsum sumcheck / 单个 claim 的 batching sumcheck 与 CPU 的证明消息（各轮多项式）逐轮比对；
- **L3 chunk 级**：同一 chunk 的 trace 分别喂 CPU 与 FPGA，产出的 ChunkProof 结构体逐字段相等；
- **L4 端到端**：FPGA 产出的 Proof 通过 CPU `Verifier::verify_streaming`（verifier.rs:755）。
  L4 通过即代表 transcript 等价性达成——这是最终验收标准，L1~L3 只是定位问题的手段。

---

## 3. 算子规格书

> 每个算子按统一模板给出：功能定义 → 协议作用 → 精确算法（伪代码含代码出处）→ 数值例子 → 硬件实现建议 → 验收要点。

---

### OP-01 Fr 域算术单元

**功能**。BN254 标量域上的加/减/乘/求逆。

**协议作用**。所有算子的底层；乘法器吞吐决定 EQ/sumcheck/RLC 类带宽型算子的上限。

**规格**。

| 运算 | 输入 | 输出 | 延迟目标 |
| --- | --- | --- | --- |
| add/sub | 2×Fr(Montgomery) | 1×Fr | 1 cycle 组合或 2 级 |
| mul | 2×Fr(Montgomery) | 1×Fr | CIOS 流水， initiation interval = 1 |
| inv | 1×Fr | 1×Fr | 低频，允许数百周期 |

- **域阶 r（源码为准：`curves/bn254/src/fields/fr.rs:4`）**：
  `r = 21888242871839275222246405745257275088548364400416034343698204186575808495617`，254 bit（2^253 < r < 2^254），MontBackend 4×64-bit limb。
  ⚠ **勿与基域 Fq 混淆**：Fq（G1 坐标域）= `21888242871839275222246405745257275088696311157297823662689037894645226208583`（`fq.rs:4`），其十六进制 `0x30644E72E131A029B85045B68181585D2833E84879B9709143E1F593F0000001` 是网上最常见的"BN254 常数"——**那是 Fq 不是 Fr**。证明算术全部在 Fr 上（v1.0 曾误引该 Fq 十六进制，v1.1 修正）；
- **Montgomery 参数**：R = 2^256（4 limb）；INV = −r^(−1) mod 2^64 与 R² mod r 由 `#[derive(MontConfig)]`（fr.rs:3）自动生成——交叉验证法：实现后做 to/from 往返（`Fr::from(Montgomery(x))` 还原 x）与随机 10⁶ 对乘法对拍；
- Montgomery 参数：R = 2^256，n' = −p^{-1} mod 2^64（取 ark-bn254 发布常数，勿自行推导后不做交叉验证）；
- **inv 的两种实现**：`a^(p−2)`（用 mul 链 + 加权位平方，254 次平方 + 约 120 次乘）或扩展欧几里得。出现位置：multiplicity 的 `F::from(count).inverse()`（lookup/context.rs:345-353）、shift 求值逆（generic_prove.rs:305-310）、iFFT scale（prover.rs:519-521）。吞吐需求 ≤ 每 chunk 数百次，无需专用高速通路。

**验收**。随机 10^6 对 (a,b)：mul/add/sub 与 CPU `ark_bn254::Fr` 逐位一致；边界 {0, 1, R, p−1}。

---

### OP-02 i64→Fr 域化

**功能**。把 trace 中的 i64 张量逐元素转为 Fr（Montgomery），并 pad 到 2^n。

**协议作用**。步骤 1 的数据底座：所有 witness 表、边界表都由它产生（lookup/context.rs:233 起的 `generate_lookup_witness_for_chunk` 内的 `to_field().pad_next_power_of_two().into_mle()`）。

**精确算法**。

```
for each i64 x:
    u = x >= 0 ? x : p - |x|          # 64 位补码取绝对值，|x| < 2^63 < p
    out = u -> Montgomery             # 一次 mul: u * R² mod p
pad: 追加 (2^n − N_orig) 个 Montgomery(0)
```

- 步骤 4 逐节点会**再次**域化该节点的输出张量（prover.rs:750-754"随用随转"）——硬件上建议把步骤 1 已域化的表缓存复用，或提供"表流入→直接喂 sumcheck"的通路，避免二次 32B/元素搬运。

**规模**。每 chunk 的 witness 元素总量 ΣQ_chunk（GPT-2 级单 chunk 可达 10^7~10^8 元素）。纯带宽算子：读 8B、写 32B / 元素。

**验收**。边界值 {0, 1, −1, i64::MAX, i64::MIN, ±量化边界}；pad 段全零；与 CPU dump 逐位一致。

---

### OP-03 Blake3 transcript / 挑战采样

见 §2.3 的完整规范。硬件要点补充：

- **核形态**：BLAKE3 压缩函数（7 轮 round、16×32-bit lane）单核即可满足带宽（每轮 sumcheck 数百字节）；关键是**低延迟出 16B XOF**；
- **必须支持的序列**：`update → update → … → finalize_xof(16B) → update → …`（吸收与采样交替；finalize 后继续 update 是 blake3 原生支持的行为，CPU 测试 `dp-crypto/src/arkyper/transcript/blake3.rs` 的 test_transcript 依赖它）；
- **逐字节复刻清单**（附录 A）：DST、标签、Fr/G1 的序列化格式、`read_challenges` 连续同值现状。

**验收**。用附录 A 的标签序列构造固定消息流，与 CPU `Blake3Transcript` 的挑战序列逐位一致（含"连续两次 challenge_scalar 相同"这一用例）。

---

### OP-04 EQ 表生成（`evals`）

**功能**。输入点 `r = (r₀,…,r_{n−1})`（n 个 Fr），输出 2^n 表 `T[i] = eq(r, i)`，eq 定义见配套详解 §2.4。可选缩放因子 s（`evals_with_scaling`，eq.rs:33-37）。

**协议作用**。每个 claim 生成一张 EQ 表（步骤 3 的 `compute_claim`、步骤 7 的 batching sumcheck、einsum 聚合）；"在点 r 求值"恒等式 `f(r) = Σ_x f(x)·eq(r,x)` 全靠它（OP-05）。

**精确算法**（eq.rs:48-70，逐行对应）：

```
T[0] = s                                   # s = 1 或缩放因子
size = 1
for j = n−1 downto 0:                      # 从点的最后一个坐标开始
    size *= 2
    for i = size−1 downto 1, step −2:
        scalar  = T[i/2]
        T[i]   = scalar * r[j]
        T[i−1] = scalar − T[i]
```

**为什么最终布局是"位 b ↔ r[b]"**：第 1 轮（j=n−1）把 r[n−1] 放在 2 元表的位 0；此后每轮 size 倍增时，旧表整体"左移一位"（旧下标 k 展开为新下标 2k 与 2k−1），新坐标 r[j] 占位 0。处理完全部坐标后，最先处理的 r[n−1] 被推到最高位，最后处理的 r[0] 留在位 0。**硬件按循环逐拍实现即可，不必自行重排**。

**数值例子**（r = (5, 4)，n = 2，可直接作测试向量）：

```
j=1（r[1]=4）: size=2 → T = [1−4, 4] = [−3, 4]
j=0（r[0]=5）: size=4 → i=3: T[3]=4·5=20, T[2]=4−20=−16
                       i=1: T[1]=(−3)·5=−15, T[0]=−3−(−15)=12
输出 T = [12, −15, −16, 20]
自检：Σ T = 1 恒成立（EQ 表元素和恒为 1）
```

**硬件建议**。写带宽主导：2^n×32B 顺序写。n 最大 ~24（张量变量数），表不要求常驻——生成后立即被 OP-05/OP-07 消费。乘法数 = 2^n − 1，每拍 1~多个乘法器即可；亦可用"分治分块生成"提高并行写口数量。

**验收**。n ∈ {1, 2, 8, 20}；点含 0/1 值坐标（退化布尔点 → 表应是单位冲击）；缩放因子版本。

---

### OP-05 MLE 求值 / EQ 加权点积

**功能**。输入表 f（2^n Fr）与 EQ 表（同长），输出 `Σ_i f[i]·T[i]`。

**协议作用**。`compute_claim`（iop/mod.rs:201-212：先 OP-03 采样点，再本算子求 eval）、batching sumcheck 的 claimed sum 核算、verifier 侧同点重算。

**精确算法**。CPU 用 `evaluate_at_chi`（dense.rs:741-743）= 纯点积（内部有 0/1 系数优化 `mul_01_optimized`，硬件可不做该优化，结果一致）；`evaluate(r)`（dense.rs:584-601）先用 OP-04 生成 EQ 再走"两半点积"（split_eq_evaluate，:620-641），数学上与全表点积等价（§2.1 已核实两种实现的位序一致：`z bit j ↔ r[j]`）。**硬件建议统一实现为全表点积**，与 `evaluate_at_chi` 对齐即可。

**数值例子**。f = [3,1,2,0]，T = [12,−15,−16,20]（OP-04 产物）：

```
eval = 3·12 + 1·(−15) + 2·(−16) + 0·20 = 36 − 15 − 32 = −11
```

（即 f 在点 (5,4) 的取值，与配套详解 §2.3 的 sumcheck 终值一致。）

**硬件建议**。读两路表流、乘加树；纯带宽。注意参与点积的两张表必须位序一致（都来自本流水线约定即可）。

**验收**。与 OP-04 联测：随机 f、随机点，对照 CPU `evaluate`；布尔点（eval = 表中单元素）；pad 表（`unpadded_len < len` 时 CPU 的 evaluate 对 pad 坐标额外乘 `(1−r)`，dense.rs:597-600——若硬件走全表点积，须先确认 pad 部分为零且点 pad 坐标处理一致，测试向量要覆盖 padded 表）。

---

### OP-06 表折叠（`fix_low` / HyperKZG Phase 1 共用核）

**功能**。输入表 z（长 L=2^m）与标量 r，输出长 L/2 的表：

```
z'[i] = z[2i] + r·(z[2i+1] − z[2i])       （i = 0..L/2−1）
```

**协议作用**。sumcheck 每轮折叠（sumcheck/prover.rs:634 fix_var_parallel → dense.rs:494-502）；HyperKZG 打开的 Phase 1 用同一公式逐轮折叠（arkyper/mod.rs:345-355，那里变量名 point[i] 即挑战 r_i）。CPU 有两个等价实现（fix_low_mut :454 与 bound_poly_var_bot_01_optimized :494，后者带 0/1 系数快路径），语义相同。

**数值例子**。z = [3,1,2,0]，r = 5：

```
z'[0] = 3 + 5·(1−3) = −7      z'[1] = 2 + 5·(0−2) = −8
```

（即配套详解 §2.3 sumcheck 轮 1 后的折叠表 [−7,−8]。）

**硬件建议**。读 2 写 1 的流式核；每轮全表读一遍、写一半；0/1 快路径（m==0 → 直接输出 z[2i]；m==1 → z[2i]+r）可选做，等价即可。n 轮总代价 ≈ 2·2^n 次读 + 2^n−1 次写。

**验收**。r ∈ {0, 1, 随机}；L=2；与 fix_low_parallel 逐位一致；连做多轮与 CPU 状态一致。

---

### OP-07 sumcheck 轮引擎

**功能**。给定：参与表集合 {f_j}（2^n 各）、单项式列表（每个单项式 = 至多 6 张表的一次逐点乘，带标量系数；来自虚拟多项式表达式编译）、claimed sum Y、max_degree d。输出：n 轮消息（每轮 d+1 个 Fr 的轮多项式取值）+ 最终挑战点 + **每张参与表在最终点的取值**。

**协议作用**。全部 sumcheck 实例（einsum 主 sumcheck、input aggregation、same_poly、linking sumcheck、步骤 7 batching sumcheck、LogUp 每轮的子 sumcheck）共用这一个引擎。CPU 黄金实现：dp-crypto `sumcheck/prover.rs:116` `IOPProverState::prove`。

**精确算法（单实例，串行版；两阶段并行见下）**。每轮 i（i = 0..n−1）：

1. **（i>0 时）折叠**：用上一轮挑战 r_{i−1} 对每张"仍是满尺寸"的表执行 OP-06（CPU 里 `fix_var_parallel`，sumcheck/prover.rs:634-653；**只折叠 `num_vars` 仍等于当前期望值的表**，较小的表——如尺寸不足的 EQ 表——按 `PolyMeta` 跳过，prover.rs:640-642）；
2. **构建轮多项式**（build_uni_poly，:528-577 + `sumcheck_code_gen!` 宏）：把当前表按**相邻对**切分（位 0 是本轮变量 X）：对每个对 b，取每张参与表的 (偶, 奇) 值 (a_j, b_j) = (f_j[2b], f_j[2b+1])，计算乘积多项式

   ```
   ∏_j ( (1−X)·a_j + X·b_j ) = Σ_{k=0..deg} c_k·X^k
   ```

   的幂基系数（度 ≤ 6，宏展开硬编码；宏用预计算差值 `c = v[b+1]−v[b]` 的二进制组合来减少乘法次数，sumcheck_macro/src/lib.rs:103-175——硬件实现任意等价算法均可，**输出必须是同一多项式**），把 c_k 按标量系数累加进轮多项式；
3. **外推**（extrapolate.rs:1-60）：若某单项式度数 deg < max_degree，轮多项式需要 max_degree+1 个点（X = 0..max_degree）。用预计算的**重心插值系数**把已知的 deg+1 个点补齐到 max_degree+1 个点：`f(z) = Σ_j v_j·f(j)`，系数表在 CPU 侧按 (min_degree, max_degree) 预生成后**可作为常量烧进硬件**；
4. **吸收与采样**：`append_scalars(轮多项式)`（注意 CPU 先 append 了 `num_variables + log2(线程数)` 与 `max_degree` 两个 u64 LE，sumcheck/prover.rs:155-156，:417-418——**这两个 8 字节头不能漏**）；`append_and_sample(b"Internal round")` 出本轮挑战；
5. n 轮后：`challenges = [r_0..r_{n−1}]`；对每张表执行最后一次折叠（sumcheck/prover.rs:63/216）后取 `z[0]`，即该表在最终点的取值（`get_mle_flatten_final_evaluations`，:587-602）。

**两阶段并行（分布式 sumcheck，arXiv:2210.00264）**：CPU 把超立方体按 P = 2^p 个线程切成子立方，Phase 1 各线程在自己的子立方上独立跑 n 轮（每轮把 P 份轮多项式**加法归约**后统一吸收采样，Phase1Workers::run，:23-71），Phase 2 合并状态后对剩余 p 个"线程选择变量"继续普通轮次（:196-208）。**硬件映射建议**：P = 计算阵列数，每阵列持表的一个连续切片；每轮做一次 P→1 的 (d+1) 项归约；Phase 2 数据量已缩小 P 倍，可在单阵列完成。注意 Phase 1 各线程切的是**表的高位连续段**（`get_batched_polys` 连续切分），局部位 0 = 全局位 p。

**串行依赖（时序预算核心）**。轮 i 的折叠必须等轮 i−1 的挑战，挑战必须等轮多项式算完并过 OP-03。**单实例内逐轮串行是协议固有属性**，不可消除；可并行的是：多实例间（不同节点/不同表的 sumcheck 相互独立）、轮内数据通路。延迟预算 ≈ n × (轮多项式遍历时间 + Blake3 + 折叠遍历时间)。

**claimed sum 约定**。引擎**不**把初始 claimed sum 吸进 transcript——它由验证方从已持有 claim 重算（einsum：γ·y_out），或由证明方先行吸收（LogUp 实例输出，logup_gkr/prover.rs:163-166）。硬件调度器必须按具体调用点区分（清单见集成章节）。

**数值例子（单实例全流程，可直接作 L2 测试向量）**。表 f = [3,1,2,0]（单项式 = 单表，d = 1…为覆盖外推路径取 max_degree = 2，即每轮发 3 个点）：

```
轮0：build_uni_poly：对 (a,b)∈{(3,1),(2,0)}：∏((1−X)a+Xb) = a + X(b−a)
     累加 → g₀(X) = 5 − 4X（度1）→ 外推补 X=2：5−8 = −3 → 发 [5, 1, −3]
     吸收 [5,1,−3]（前置 u64 头）→ 挑战 r₀ = 5
轮1：OP-06 折叠 → [−7,−8]；build → g₁(X) = −7 − X → 补 X=2：−9 → 发 [−7,−8,−9]
     校验（V 侧）：−7+(−8) = −15 = g₀(5) ✓ → 挑战 r₁ = 4
终值：折叠 [−7,−8] with 4 → −7 + 4·(−1) = −11
最终取值：f(5,4) = −11（与 OP-05 例子一致 ✓）
```

**验收**。L2：与 CPU 同输入（表、单项式表、max_degree、transcript 初始态）逐轮比对消息与终值；覆盖 deg < max_degree 的外推路径、多单项式、PolyMeta 混合尺寸表、P>1 两阶段。

---

### OP-08 RLC（随机线性组合 `linear_combination`）

**功能**。输入 k 张表 {P_i}（可为不同长度的 padded 表）与 k 个标量 c_i，输出表 `rlc[i] = Σ_i c_i·P_i[i]`（对 i 超出某表 unpadded_len 的位置该表贡献 0）。

**协议作用**。步骤 7：把 batching sumcheck 归单点后的所有被引用表塌成**一条**多项式，做唯一一次 KZG 打开（poly_commit/prover.rs:115-131；实现 dense.rs:934-950：逐坐标 `if i < poly.unpadded_len() { acc += poly[i]·c }`）。

**精确算法**。如上，纯逐元素。注意：输出长度 = 最长输入的**padded** 长度；短表的高段贡献 0。

**硬件建议**。k 路表流 + k 个系数寄存器 → 乘加树 → 单路输出流。一次扫描全部 witness 数据（这是数据量与 witness 承诺同量级的最后一遍读）。

**验收**。k=1/2/多表不同长；系数 0/1；与 CPU 逐位一致。

---

### OP-09 einsum 预处理（Outer 轴固定与切表）

**功能**。输入：某 einsum 节点的输入张量表（已域化）、输出 claim 点按维切段、各轴的 Outer/Stacked/Contracted 标注（由 CPU 下发）。输出：每张参与 sumcheck 的"固定后子表"（长 2^ceil_log2(收缩维)，每个 stack 取值一张）+ stacking 系数。

**协议作用**。einsum sumcheck 的建表阶段（`mles_from_tensor`，axis.rs:1142-1234；`sort_variables_to_axes` :503-580 负责标注）。sumcheck 只在收缩维上跑，Outer 维必须预先用 EQ 加权点积消去。

**精确算法**（从最后一个维度向前扫描；axis.rs:1152-1214）：

```
current = 展平表(截到 unpadded 形状)
chunk_size = 1; contraction = 1
for (fix, dim) in zip(各维标注, 形状) REVERSED:      # 最后维最先处理
    match fix:
      Outer(β):                      # β = 该维坐标段的 EQ 表(eval 结果)
        if chunk_size == 1:          # 直接点积分支 axis.rs:1158-1170
            current = for each 连续块(dim 大小): Σ v·β
        else:                        # stride 点积分支 axis.rs:1178-1195
            for each 大块(chunk_size·dim):
                for i in 0..chunk_size:
                    out[i] = Σ_{t} current[i + t·chunk_size] · β[t]
        （chunk_size 不变）
      Stacked:    chunk_size *= dim  （数据不动）
      Contracted: chunk_size *= dim; contraction *= dim  （数据不动）
最后：按 contraction 切成 chunk_size 张表，各补零到 2^ceil_log2(contraction)
```

stacking 系数 `stacking_coeffs[j] = eq(stack点, j)`（axis.rs:1043-1062，EQ 表的逐维乘积展开）。

**数值例子**（与配套详解 §5.5.3 完全一致，可整段作测试向量）。X=[[1,2],[3,4]]（变量 (a,k)，行主序展平 [1,2,3,4]），W=[[5,6],[7,8]]（变量 (k,j)，展平 [5,6,7,8]）。输出 claim 点 (a=2, j=3)。轴：a→Outer(β_a=eq表(2)=[−1,2])，j→Outer(β_j=eq表(3)=[−2,3])，k→Contracted。

- W（按 rev 顺序先处理最后维 j，chunk_size==1 分支）：连续块 [5,6]、[7,8] 与 β_j=[−2,3] 点积 → R = [−10+18, −14+24] = [8, 10]；
- X（后处理行维 a，此时 chunk_size=2，stride 分支）：大块 [1,2,3,4]，i=0：取 [1,3]·[−1,2] = 5；i=1：取 [2,4]·[−1,2] = 6 → L = [5,6]。

后续 sumcheck 与收 claim 见配套详解（Σ L·R = 100 = Y(2,3)；输入 claim X(2,4)=9；权重 claim W(4,3)=16）。

**硬件建议**。数据重排为主（stride 读取模式），乘加为辅；建议做成"可配置维度扫描器"：按 CPU 下发的 (维度大小, 类型, β表指针) 描述符工作。Stacked/Contracted 维不产生计算，只是布局元数据。

**验收**。覆盖两个分支（chunk_size==1 / >1）、多维（≥3 维、多个 Contracted）、pad 前截断（reduce_to_shape，axis.rs:1148）；与 CPU `fix_axes` 输出逐位一致。

---

### OP-10 批量 MSM（Pippenger，共享基点）

**功能**。输入：基点表 `g1_powers[0..N]`（SRS，仿射 64B/点）与 M 组标量集 {s_i[0..N']}（N' ≤ N，每组 = 一张表的 2^n 个 Fr），输出 M 个 G1 点 `C_i = Σ_j s_i[j]·G_j`。

**协议作用**。两个主算场景：**witness 批量承诺**（步骤 1，poly_commit/context.rs:58-88 → arkyper/mod.rs:471-483：所有 witness 表切 2^20 块后一次调用）与 **HyperKZG Phase 2**（步骤 7，对折叠中间多项式承诺，arkyper/mod.rs:362）。**标量就是表的元素本身**（`batch_poly_msm_cpu` 直接取 `evals_ref()`，msm.rs:47-53）。

**切块规则**（调用方已完成，硬件按块接收即可）：`num_chunks = ceil(2^n / 2^20)`，上限 128/表（poly_commit/context.rs:103-121）；SRS 长度按最大块变量数生成（context.rs:161-166），每块用 `g1_powers[0..2^20]`。

**精确算法（Pippenger 桶法）**。单 MSM：把每个 254-bit 标量切成 ⌈254/w⌉ 个 w 位窗口；对每个窗口 t：清桶数组 B[0..2^w−1]，把点 G_j 加进桶 B[scalar_j 的第 t 窗口值]；然后桶内求和、按 2^{w·t} 位权合并。复杂度 ≈ N·(254/w) 次点加。

**批量加速（硬件的重点机会）**。`batch_poly_msm` 的 M 组标量**共享同一基点表**：基点只需从 HBM 读一遍，为每组标量维护一套桶（或时分复用桶阵列）即可同时/流水完成 M 个 MSM。GPU 版（gpu_msm.rs:112 `msm_arc`）即按"基点常驻、标量成批"组织，FPGA 可对齐该接口形态。w 的选取平衡桶存储（BRAM）与点加次数：2^20 标量、w=10~12 时桶表 = 2^w×(254/w) 个部分和，是主要的片上存储开销。

**点运算注意**。桶累加建议用射影/扩展坐标避免逆元；最终一次仿射化（每 MSM 一次求逆 + 少量乘）。点加/倍增公式用标准 BN254 G1（a=0 短 Weierstrass）。

**规模与内存**。SRS 常驻：2^20 × 64B = 64 MiB（HBM）。单块标量：2^20 × 32B = 32 MiB 流式喂入。GPT-2 级 witness 总量：1.24 亿参数 × 32B ≈ 4 GB 标量流（≈ 124 个 2^20 块）。

**验收**。与 CPU `VariableBaseMSM` 逐位一致（点坐标相等）；标量含 0/1/满值；M=1 与 M>1 结果一致；块长 2^20 与小块（测试用 2^8）双配置。

---

### OP-11 HyperKZG 打开（三 Phase 全链）

**功能**。输入：表 f（长 2^ell，即 rlc 多项式）、打开点 r（ell 个 Fr）、SRS。输出：`HyperKZGProof{coms: ell−1 个 G1 点, w: 3 个 G1 点, v: 3×ell 个 Fr}`。

**协议作用**。步骤 7 的最后一步：向验证方证明"rlc 在点 r 处 = v_claim"，且该承诺与 setup 绑定（poly_commit/prover.rs:125-131 → arkyper/mod.rs:497-506 → open :329-383）。每 chunk 一次。

**精确算法**（全部自 arkyper/mod.rs:329-383、:186-234 核实）：

**Phase 1 —— 折叠（复用 OP-06）**：`polys[0] = f`；i = 0..ell−1：`polys[i+1][j] = r_i·(polys[i][2j+1] − polys[i][2j]) + polys[i][2j]`。得到 ell 张表（最后一张长 2）。

**Phase 2 —— 折叠承诺（复用 OP-10）**：对 `polys[1..]`（ell−1 张）做一次 `batch_poly_msm` → `coms`；吸进 transcript；采样 r → 三个打开点 `u = [r, −r, r²]`（:372-373）。

**Phase 3 —— 多点批量打开（kzg_open_batch，:186-234）**：

1. 对每张折叠表 p_j（j=0..ell−1）、每个点 u_i（i=0..2）：`v[i][j] = eval_as_univariate(p_j, u_i)`——**把表当一元多项式系数**，用增量幂累加 `out = z[0]; rpow = u; for z in z[1..]: out += rpow·z; rpow *= u`（dense.rs:765-773）。硬件：一张表流过时同时维护 3 个 (累加器, 幂) 对，一次扫描出 3 个值；
2. 吸收全部 v → 采样 q → `q_powers = (1, q, …, q^{ell−1})`（transcript/mod.rs:36-41）；
3. `b_poly = Σ_j q^j·polys[j]`（又一轮 RLC，复用 OP-08，长度 = 2^ell）；
4. 对 b_poly 在 3 个点做**无余式商多项式**（kzg_batch_open_no_rem，:186-234）：对每个 u_i，`h_i = (b − b(u_i))/(X − u_i)` 的系数用综合除法递推

   ```
   h[len−1] = 0
   for i = len−1 downto 1:  h[i−1] = b[i] + h[i]·u_i      （mod.rs:171-182）
   ```

   每个 h_i 是 (2^ell − 1) 长的表，共 3 张，一次 `batch_poly_msm` 出 `w = (w₀,w₁,w₂)`；
5. 吸收 w，采 d₀（仅推进 transcript 状态，:230-231）。

**数值例子（Phase 1）**。f = [3,1,2,0]，r = (5,4)：折叠轮 1 → [3+5·(1−3), 2+5·(0−2)] = [−7,−8]；轮 2 → [−7 + 4·(−8−(−7))] = [−11]。与 f(5,4) = −11 一致（验证方自算末值，故代码不发最后折叠的承诺）。

**验证侧对照（硬件不需要实现，仅用于联调理解）**：一致性检查 `2r·Y[i+1] = r(1−x_i)(ypos+yneg) + x_i(ypos−yneg)`（mod.rs:420-428）+ 一次 MSM + 一次配对等式（:237-320）。

**硬件建议**。Phase 1/3 的表操作全部流式；Phase 2 与 Phase 3.4 各是一次共享基点批量 MSM（复用 OP-10）。数据量 ≈ 2·2^ell（折叠）+ 3·2^ell（三点求值）+ 3·2^ell（witness）次表扫描 + 2 次 MSM（ell−1 张小表 + 3 张大表）。

**验收**。ell ∈ {1, 2, 8, 20}；与 CPU `HyperKZG::open` 的 (coms, w, v) 逐位一致；并用 CPU `HyperKZG::verify` 回验通过。

---

### OP-12 查表 witness 生成（位切片 + 值计数 + multiplicity）

**功能**。输入：某查表层节点的输入张量（i64）与表定义。输出：位切片列 MLE 组、输出列 MLE、每表值被查计数、multiplicity 表。

**协议作用**。步骤 1 的 witness 与步骤 6 的 LogUp 表侧输入（lookup/context.rs:233-362）。

**精确算法**：

1. **位切片**：按表位宽把每个 i64 拆成若干列（如 GELU：高/低位列 + 输出列；activation.rs:313-344 登记到 shift_check / 主表 / zero_check / signed_zero_check 四张表）；
2. **值计数**：`HashMap<值, 次数>` 统计（lookup/context.rs:313 附近的 element_counts）；
3. **multiplicity**：对表的**每个位置** j（值 v_j）：`mult[j] = 被查次数(v_j) × inverse(表内出现次数(v_j))`；未查过的值为 0（lookup/context.rs:317-362）。表 pad 产生的重复值已按"出现次数"归一。自检：`Σ mult[j] = 查询总数`。

**数值例子**。表 T = [10,20,10,30]，查询 q = [20,10,10]：

```
被查次数：10→2, 20→1, 30→0；出现次数：10→2, 20→1, 30→1
mult = [2/2, 1/1, 2/2, 0/1] = [1, 1, 1, 0]    Σ = 3 = |q| ✓
```

**硬件建议**。计数是哈希/直方图问题，硬件实现代价高、带宽小（i64 输入、输出仅表长）——**建议留 CPU**，仅把产出的列 MLE 交给 OP-02/OP-10。位切片是纯位操作，若要下沉非常便宜。

**验收**。与 CPU gen_lookup_witness 的 (列 MLE, counts, mult) 一致；覆盖 pad 重复值、负值、未查值。

---

### OP-13 LogUp GKR 轮引擎（P2，可后置）

**功能**。对一批同变体的查表实例，证明"分数和恒等式"：把每次查表/每个表位置表示为分数，构造逐层分数加法的 GKR 电路，用逐轮 sumcheck 证明电路计算正确。

**协议作用**。查表层的查询侧（步骤 4，generic_prove.rs:133）与表侧（步骤 6，prover.rs:266）共用同一批量引擎 `new_batch_multiple_sizes_prove`（logup_gkr/prover.rs:91-344）；电路结构与层表达式定义在 `lookup/logup_gkr/circuit.rs`，输入构造在 `structs.rs`。

#### 13.1 电路模型（circuit.rs:17-201）

**层状态**。电路自底向上由三类层组成：

| 层类型 | 状态 | 语义 |
| --- | --- | --- |
| `InitialLookup` | 分母列组 `D_c`（c = 0..C−1，各长 L）+ β + α_col | 每个查询位置 p 的分数 = `−1 / (β + Σ_c α_col^c·D_c[p])`，分子恒为 −1 |
| `InitialTable` | 分子 `mult`（长 L）+ 分母列组（表列）+ β + α_col | 每个表位置 p 的分数 = `mult[p] / (β + Σ_c α_col^c·D_c[p])` |
| `Generic` | (numerator, denominator) 各长 L′ | 中间层：已折叠的分数对 |

**逐层递推 `next_layer`**（circuit.rs:88-184）。把当前层对半分（half = L/2），位置 i 与 i+half 配成一个分数加法：

```
底 → 第一 Generic 层（:117-183）：配对双方分母先做"列合并"再相加
     D̄(p) = β + Σ_{c=0..C−1} α_col^c · D_c[p]        （:129-141 / :159-171 的 fold）
     num' = n1·d2 + n2·d1        den' = d1·d2         （Fraction 加法，3 乘 1 加 / 对）
     查询侧 n1 = n2 = −1；表侧 n = mult 值
Generic → Generic（:97-116）：同样对半配对、直接分数相加（无列合并）
```

**电路输出**（:186-201）。折叠到长度 2 的 Generic 层即顶：

```
outputs = [num₀, num₁, den₀, den₁]          （4 个 Fr，证明开始前吸进 transcript，prover.rs:163-166）
claimed 分数和 = (num₀·den₁ + num₁·den₀) / (den₀·den₁)
```

**数值例子（可直接作测试向量）**。

查询侧：1 列、4 个查询值 `q = [1,2,3,4]`，β = 0、α_col = 1：

```
配对 (1,3): (−1/1) + (−1/3) = (−4, 3)     配对 (2,4): (−1/2) + (−1/4) = (−6, 8)
Generic 层 = { num = [−4,−6], den = [3,8] }
outputs = [−4, −6, 3, 8]；分数和 = (−4·8 + −6·3) / (3·8) = −50/24 = −25/12
直算核对：−1/1 − 1/2 − 1/3 − 1/4 = −25/12 ✓
```

表侧：表列 `[1,2,3,4]`、multiplicity `mult = [2,0,1,0]`，β = 0、α_col = 1：

```
配对 (位置0, 位置2): (2/1) + (1/3) = (7, 3)     配对 (1,3): (0/2) + (0/4) = (0, 8)
outputs = [7, 0, 3, 8]；分数和 = (7·8 + 0·3)/24 = 7/3
直算核对：2/1 + 1/3 = 7/3 ✓
```

#### 13.2 每轮 sumcheck 的精确表达式

**挑战语义（前置）**：`Expression::Challenge(id, pow, s, off)` 求值为 `challenges[id]^pow · s + off`（dp-crypto `sumcheck/expression/utils.rs:89-93`）。LogUp 每轮传给 sumcheck 引擎的挑战数组 = `[alpha, lambda]`（prover.rs:272-273），claim 求值时 = `[alpha, lambda, batching_challenge]`（prover.rs:55）。因此：

- `Challenge(0, k)` = **alpha^k**，k = 实例在轮内的序号（用幂区分同轮批量多实例）；
- `Challenge(1, 1)` = **lambda**（分子/分母合并系数）；
- `Challenge(2, 1)` = **batching_challenge**（claim 点末坐标，合并低/高半区）。

**轮结构**（prover.rs:183-297）。轮 l（l = 1..=total_layers，total_layers = 各实例变量数最大值）：

1. 活跃实例 = 变量数 ≥ 剩余层数者；小实例本轮"滚入"，其 outputs 并入 wit_evals（:183-203）；
2. 每个活跃实例取**下一个待证层**（自顶向下迭代器 :146/:227-245），构造该层的半表 MLE 组：Generic → 4 组（num_low/num_high/den_low/den_high，各长 half）；InitialTable → 2 组分子 + 2C 组分母；InitialLookup → 仅 2C 组分母（分子 −1 以常数折进表达式，:394-401）；
3. 从当前 `sumcheck_point` **尾部**取坐标生成 EQ 表（:247-248），乘到表达式上（:60-68 full_sumcheck_expression）；
4. 各活跃实例表达式相加成**一个**虚拟多项式，跑一次 OP-07（:263-275）；
5. 结束后：各实例 4 个半表在挑战点的取值进入 wit_evals（:281）；采样新 `batching_challenge` 续到点尾（:284-291）；非末轮重采 alpha/lambda（:286-289，标签 `b"logup_*"`）。

**sumcheck 表达式**（circuit.rs:279-284 / :339-344 / :397-401；记 D̄_low(x) = β + Σ_c α_col^c·col_c,low(x)，D̄_high 同构）：

```
Generic:        α^k · ( N_low·D_high + N_high·D_low + λ·D_low·D_high )
InitialTable:   α^k · ( N_low·D̄_high + N_high·D̄_low + λ·D̄_low·D̄_high )
InitialLookup:  α^k · ( −D̄_high − D̄_low + λ·D̄_low·D̄_high )
```

**claim 表达式**（circuit.rs:241-253，作用于上层 4 值 `[N_low, N_high, D_low, D_high]`——首轮即 outputs，后续轮为上一轮的 4 个半表终值）：

```
claim_k = α^k · ( bc·(N_high−N_low) + N_low + λ·( bc·(D_high−D_low) + D_low ) )
        = α^k · ( N(r) + λ·D(r) )          # bc 合并低/高半区
```

**协议不变量**：轮 l 的 claimed sum = Σ_k claim_k；而 sumcheck 表达式恰是"下一层值的 N′ + λ·D′"（N′ = N_low·D̄_high + N_high·D̄_low，D′ = D̄_low·D̄_high——即分数加法结果的分子/分母），EQ 加权求和后等于 claim。小实例的 claim 额外乘 `2^(total_layers − size)` 对齐量纲（prover.rs:221-222）。

**数值例子（表侧单实例 round-1 全链）**。沿用 13.1 表例（mult=[2,0,1,0]、列 [1,2,3,4]、β=0、1 列）。total_layers = 1，round 1 即末轮。待证层 = InitialTable，half = 2，半表：

```
N_low = [2,0]   N_high = [1,0]   D_low = [1,2]   D_high = [3,4]
D̄_low(x) = 0 + D_low(x)          D̄_high(x) = 0 + D_high(x)
sumcheck 表达式 g(x) = N_low·D̄_high + N_high·D̄_low + λ·D̄_low·D̄_high：
  g(0) = 2·3 + 1·1 + λ·(1·3) = 7 + 3λ
  g(1) = 0·4 + 0·2 + λ·(2·4) = 8λ
claimed sum = claim_0（bc = 初始 batching 挑战）
            = bc·(0−7) + 7 + λ·(bc·(8−3) + 3) = 7 − 7bc + λ·(3 + 5bc)
自检：N′ = [7, 0]、D′ = [3, 8]（即 13.1 的 Generic 层），
     Σ_x (N′(x)+λ·D′(x))·eq(bc,x) = N′(bc) + λ·D′(bc) = claim ✓
```

**最终 claims**（prover.rs:307-330 + circuit.rs:442-500 `combine_final_evals`）。末轮结束时，把**底层半表终值**按最终 batching_challenge 合并：

```
InitialLookup（C 列）：2C 个终值 → 每列 claim = bc·(col_high@r − col_low@r) + col_low@r
    → 对 witness 输入切片列的 claim（步骤 7 打开 witness 承诺，generic_prove.rs:272-274）
InitialTable：2(C+1) 个终值 → [multiplicity claim] + [各列 claim]
    → mult 进 witness claim；承诺过的表输出列进表 claim（prover.rs:288-313）
claim 点 = sumcheck_point 的最后 (num_vars+1) 个坐标（:316/:324）
```

#### 13.3 硬件实现建议

- **每轮 = 一次 OP-07 调用**。层表达式在 CPU 侧是 Expression 树；硬件侧**要求 CPU 预编译成单项式调度表**（(标量系数, 表 id 序列) 列表）下发，FPGA 不实现通用表达式解释器。`circuit.rs:231-412` 的三个表达式模板即编译器的对拍样例；
- **层折叠核**（witness 侧生成下一层）：逐对分数加法 3 乘 1 加，纯流式；多列合并 D̄ 与折叠融合为单遍扫描；
- **wit_evals / claim 链**：每轮 4×活跃实例数 个 Fr 的标量往返走 OP-03；
- **分期策略**：一期 CPU 跑 LogUp 全链，FPGA 只承接其调用的 OP-04/06/07 算力（LogUp 的每轮就是标准 sumcheck）；二期下沉"层折叠 + 单项式预编译"，OP-07 复用主引擎。

#### 13.4 验收

- 13.1 / 13.2 的三组数值例子作为最小测试向量；
- 配置覆盖：单实例（1 列 / C 列）、多实例同轮、异构尺寸滚入；与 CPU `LogUpBatchProof` 逐字段一致；
- 查询侧与表侧输出 claim 配对后分数恒等式闭合（对照 verifier.rs:587 的配对检查）；
- 查询侧列 claim 与 witness 承诺打开值一致（经步骤 7）。

---

### OP-14 卷积 FFT 矩阵委托（P3，按需）

**功能与适用范围**。CNN 卷积层的证明。LLM 流水线（gemma/gpt2 的 einsum 路径）**不触发**本算子；仅当目标模型含 Conv 层时需要。推理期该层本身以 FFT 卷积计算（`fft_conv`，convolution/mod.rs:672-690，proving_data 携带 `input_fft` / `prod` / `output_as_element` 等中间量），证明期按卷积定理分六段完成（`prove_convolution_step`，convolution/mod.rs:784-1120）。

#### 14.1 协议总览（六段）

设输入 X（3D：kx×n×n）、滤波器 W（4D：kw×kx×nw×nw），卷积定理：`Y[i] = iFFT( Σ_{j∈[kx]} FFT(X[j]) ⊙ FFT(W[i][j]) )`。

| 段 | 内容 | 代码 |
| --- | --- | --- |
| S0 | 垃圾清除：output = conv_after_bias ⊙ clearing（0/1 张量，padding 区为 0）。一次 hadamard sumcheck（两表乘积），产出双 claim：v1（未清除张量）继续供后续段使用，clearing 张量 claim 由验证方人工核对 | mod.rs:805-828 |
| S1 | bias 拆分：把 claim 点重排为 r（前缀坐标取 `1−p`，:850-856），bias 在 bias_point 上求值 → bias claim（对 setup 承诺） | mod.rs:830-868 |
| S2 | iFFT 批量 sumcheck：证 `y = Σ_i W_ifft(r1,i)·PROD(i, r2)`（Y = iFFT 矩阵 × prod；W(r1,·) 由 φ 递推直接算出，是已知常量表） | prover.rs:518-577 |
| S3 | 卷积定理三表 sumcheck：`Σ_k β2[k]·x_fft[j][k]·w_red[j][k]`。w_red = 输出通道按 beta1 加权聚合的 filter（聚合 → index_wf 打包 → FFT，mod.rs:972-994）；beta_acc = EQ(r2) 重复 kx 次（:965） | mod.rs:963-1003 |
| S4 | FFT 批量 sumcheck ×2（输入 X 与聚合 filter）：证 `X_fft = FFT(r1,·) × X`。权重版 `prove_batch_fft_weights` 先把 pre-fft 滤波器按 `index_wf` 打包进 padded 行（:741-747），并产出 partial_evals 供权重 claim 使用 | prover.rs:461-516；mod.rs:698-779 |
| S5 | DFT 矩阵委托链（见 14.2），iFFT/FFT 两个方向各一条 | prover.rs:328-382 |

权重 claim 的核对逻辑（验证侧，写测试向量时对照）：取权重 sumcheck 点末坐标 v，用 `(1−v)^{-1}` 修正 padding 贡献，再用 `identity_eval` 对 `indexes`（pre-fft 位置 → 原张量位置的映射）做 EQ 加权与 partial_evals 求和核对（mod.rs:1077-1098 的 debug_assert 即完整验证式）。最终产出 **filter_claim 与 bias_claim**，经 `add_common_claims` 注册（mod.rs:1100-1107），并入步骤 7 打开 setup 承诺。

#### 14.2 DFT 矩阵委托链（delegate_matrix_evaluation，prover.rs:328-382）

FFT/iFFT 矩阵是公开常量、不做承诺，但"它在随机点 r1 处的折叠值 W(r1,i)"需要证明——zkCNN 方法：DFT 矩阵按蝶形分解成 log₂(2n) 层，逐层用一个 sumcheck 证明中间表的正确性：

- **φ 递推表**（phi_pow_init prover.rs:385-398 / phi_g_init :402-455）：`omegas[i] = φ^i`，φ = 2n 次单位根（iFFT 方向取逆根并乘 1/N 缩放 :519-521）；`phi_g_init` 从 r1 出发做蝶形递推，**同时**产出 W(r1,·)（w_red，sumcheck 的常量表）与各中间层表 `f_middle[l]`（:426-446 / :444-445）；
- **逐层 sumcheck**（:342-380）：对层 l 自顶向下共 `r1.len()−1` 轮：

  ```
  φ_l 表：由 omegas 与 r1、上一轮挑战点 r2 逐元素递推（:346-358；
          iFFT 末层用特殊形式 (1−r2[last]) 组合，:347-351）
  beta  = evals(r2 去尾一位)                       （:344）
  sumcheck：Σ_x beta(x)·φ_l(x)·f_middle[l](x) = claim    （三表乘积 → OP-07）
  r2 ← 本轮挑战点，进入下一层                       （:376）
  ```

- **落地**：链的末端回到 omegas 表（2 的幂次、公开常量）——验证方自行重构核对，链不需要任何承诺。

#### 14.3 硬件算子清单（OP-14 子核）

| 子核 | 内容 | 复用 |
| --- | --- | --- |
| OP-14a | Fr 域 FFT 蝶形核：witness 侧 `fft_conv` 与 S3 的 aggregated_filter（zkml/src/fft.rs） | 新增 |
| OP-14b | φ 递推表生成（phi_pow / phi_g，纯乘加蝶形，小表） | 新增（小） |
| OP-14c | 两表 / 三表乘积 sumcheck（S0 hadamard、S2/S4 批量、S5 每层） | OP-07 |
| OP-14d | 委托链调度：log₂(2n)−1 个串行 OP-07 实例 + 挑战链 | OP-03 |
| OP-14e | index_wf 打包 / clearing 张量生成 | gather，纯搬运 |

#### 14.4 数值例子（最小恒等式，可作单元测试）

N=2 FFT（ω = −1）：`X = [a, b] → X_fft = [a+b, a−b]`。卷积定理核对：X = [1,2]、W = [3,0]（循环卷积 = [1·3, 2·3] = [3,6]）：

```
X_fft = [3, −1]      W_fft = [3, 3]      逐点积 = [9, −3]
iFFT（÷N）= [(9−3)/2, (9+3)/2] = [3, 6] ✓
```

#### 14.5 验收

单 conv 层小模型端到端：与 CPU `ConvProof` 各段证明逐字段一致；委托链各层消息与 CPU 一致；`filter_claim` / `bias_claim` 与 setup 承诺打开值一致；S0 的 clearing claim 能被验证方的人工核对逻辑接受。

---

## 4. 集成设计

### 4.1 一次 chunk 的算子调度序列

按 `prove_chunk` 八步映射（T=表流，C=claim/控制）：

```
步骤1  [OP-12:CPU] witness切片+计数 → [OP-02] 域化全部表 → [OP-10] batch_commit
       → 承诺按序进 [OP-03]（witness → multiplicity → chunk IO，顺序固定，
         lookup/context.rs:443-482）
步骤2  [OP-03] 抽 ChallengeStorage（有查表层时）
步骤3  [OP-03]→[OP-04]→[OP-05]：模型输出 compute_claim；
       [OP-03] common_point 增长 → [OP-05] 出边求值
步骤4  逆拓扑逐节点：
       einsum：[OP-09] 建表 → [OP-03] batching 挑战 → [OP-07] 主 sumcheck
               → 收 claim（权重/偏置/输入三类分流）
       查表层：[OP-13]+[OP-07]（或 CPU 侧完成）→ witness claim
       同端口多 claim：[OP-07] same_poly 实例
步骤5  边界 claim 登记（控制面）
步骤6  [OP-13] 表侧批量 LogUp（或 CPU）→ claim 分流
步骤7  [OP-07] batching sumcheck（EQ 表=OP-04 每claim一张）→ [OP-08] RLC
       → [OP-11] HyperKZG 打开（内含 [OP-06]/[OP-10]）
步骤8  控制面组装 ChunkProof
```

### 4.2 串行依赖与挑战链

全局只有一条因果链不可并行化：**"轮多项式 → Blake3 → 挑战 → 折叠"**，存在于每个 sumcheck 实例内部。可并行维度：

1. 不同节点/不同实例的 sumcheck（步骤 4 的逆拓扑顺序只在 claim 依赖处串行——节点 A 的输入 claim 是节点 B(上游) 的输出，二者必须串行；同层多节点、以及与当前节点无关的查表证明可并行）；
2. OP-10 的 M 组 MSM（共享基点，标量流独立）；
3. Blake3 核独立于主数据通路常驻。

**延迟预算公式**（单 chunk）：T ≈ T_witness(MSM 带宽) + Σ_节点 n_i×(遍历+Blake3+折叠) + T_batch(1 sumcheck) + T_open(2 MSM + 折叠)。sumcheck 与 MSM 带宽需求同为 TB 级（GPT-2 chunk），二者共享 HBM 时注意仲裁。

### 4.3 内存预算（以 2^20 块为粒度）

| 对象 | 大小 | 常驻性 |
| --- | --- | --- |
| SRS g1_powers（2^20 仿射点） | 64 MiB | 常驻 HBM |
| 单个 2^20 Fr 表 | 32 MiB | 流式，双缓冲 ×2 |
| witness 总流（GPT-2 级 chunk） | 数 GB | 只流过一次 |
| 桶阵列（w=10，双套） | ~2^10×26×32B×2 ≈ 3.4 MB | 片上 BRAM |
| Blake3 状态 | < 1 KB | 寄存器 |

双缓冲建议：所有表级算子（OP-04/06/07/08/11）按 2^20 块或 num_vars 维度分块流水，读块 N+1 与算块 N 重叠。

### 4.4 数据量公式

- witness 域化元素总量 ΣQ（i64 个数）→ Fr 表字节 = 32·ΣQ；MSM 块数 ≈ ΣQ / 2^20；
- 单节点 sumcheck 扫描量 ≈ 4·2^n·32B（轮多项式遍历 + 折叠，n 轮几何级数求和）；
- 步骤 7 batching sumcheck 的参与表数 = 该 chunk 全部被引用表数（几十~几百），EQ 表 = 每 claim 一张（claim 数同量级）。

---

## 5. 验证计划

### 5.1 golden dump 的获取方法

CPU 侧是唯一 golden 参考。建议在 zkml 加一个仅测试用的 dump 通道（`#[cfg(feature = "fpga-dump")]`）：在下列位置把中间结构序列化到文件（格式建议：小端字节流 + 段头魔数）：

- `generate_lookup_witness_for_chunk` 末尾：全部列 MLE、counts、mult、承诺列表；
- 每次 `IOPProverState::prove` 的入口/出口：参与表引用、单项式描述、每轮轮多项式、挑战、最终取值；
- `prove_chunk` 各步骤边界：claim 寄存器快照、transcript 无法直接 dump（状态在 blake3 内部），改为 dump **吸收字节序列**（在 append_bytes 处挂钩）与每次采样的 16 字节；
- `HyperKZG::open` 出入口：f、r、(coms, w, v)。

**transcript 吸收字节序列是最重要的 dump**：L1 全部通过但 L4 失败时，二分比对两侧吸收流即可定位第一次分歧。

### 5.2 等价层次与通过标准

| 层次 | 比对对象 | 通过标准 |
| --- | --- | --- |
| L1 | 单算子 I/O | 逐位相等，≥10^6 随机用例 + 边界用例 |
| L2 | 单 sumcheck / 单 MSM / 单 open | 全部消息与产物逐位相等 |
| L3 | 单 chunk ChunkProof | 结构体逐字段相等（serde 序列化字节相等） |
| L4 | 端到端 Proof | CPU `Verifier::verify_streaming`（verifier.rs:755）通过 |

### 5.3 必测边界用例清单

- 挑战采样：连续两次 challenge_scalar 相同（blake3 XOF 位置语义）；`read_challenges(n)` 全同值；标签大小写敏感（`b"Internal round"` 含空格与大写）；
- 表：num_vars = 0/1 的退化表；padded 表（unpadded < 2^n）；全零表；0/1 系数（CPU 有 mul_01 快路径，结果须与慢路径一致）；
- einsum：收缩维 = 1；Stacked 维存在；3 输入（aggregation sumcheck）；bias 存在/不存在；
- MSM：标量 0/1/p−1；单表多块（>2^20，测试可用 2^12 阈值配置重跑）；M 组共享基点；
- HyperKZG：ell = 1（无折叠承诺，coms 为空）；r 含 0；
- chunk：单节点 chunk；含 SplitLayer/RecombinationLayer 的 chunk（claim 透传路径）；
- 查表：负值查表（补码切片 + signed_zero_check 表）；pad 重复值。

---

## 6. 风险与开放问题

1. **transcript 等价是最高风险项**：任何一处序列化差异（Montgomery 忘转 canonical、∞ 点 16B 约定、u64 头遗漏）都会静默产出"格式正确但验证失败"的证明。缓解：附录 A 清单 + 吸收字节流二分比对工具化。
2. **位序约定已定案但易被"直觉"改写**：bit b ↔ r[b]、fix_low 相邻对、evals 倍增移位——三者互相自洽，任何一处按"更自然"的方式重写（比如跨半区折叠）都会破坏与 CPU 的一致性。缓解：OP-04/06/07 的 L1 测试向量直接取自 CPU dump，不允许手造。
3. **sumcheck 逐轮串行**是协议固有延迟：单实例无法流水化挑战依赖，吞吐必须靠实例间并行 + 阵列内数据并行弥补。若端到端延迟不达标，优先扩大 OP-10 与 OP-07 的实例并行度，而非试图打破挑战链。
4. **OP-13 / OP-14 规格已在 v1.1 补全**（§3 对应小节）。残余实现风险：LogUp 与 FFT 委托链的层表达式在 CPU 侧以 Expression 树传递，硬件侧应要求 CPU **预编译为单项式调度表**（(标量系数, 表 id 序列) 列表）后下发，避免在 FPGA 实现通用表达式解释器；`circuit.rs:231-412` 的三个表达式模板就是编译器的对拍样例。另外 S5 委托链是 `log₂(2n)` 个串行 sumcheck，挑战链延迟与 OP-07 相同量级，卷积模型的端到端延迟预算需按层数线性叠加。
5. **read_challenges 同值现状**：若上游未来修复 dp-crypto 的 XOF 位置语义，挑战序列将改变——硬件的 Blake3 核应实现为可配置（位置推进与否），并在 CI 中同时跑两种模式以防协议升级造成静默失配。

---

## 附录 A：transcript 吸收与标签总表

> 语义：`DST` = transcript 构造时的初始 update；`label` = `append_and_sample` 前吸入的字节。所有 Fr 吸收 = canonical 32B LE；所有 G1 吸收 = x(32B)‖y(32B)，∞ = 16B 零；u64 = 8B LE。

| 标签 | 作用位置 | 代码 |
| --- | --- | --- |
| `b"model_proving"` | 全局/chunked transcript 的 DST（随后吸收全部模型承诺） | prover.rs:583 |
| `b"model_chunk_proving"` | 块级 transcript 的 DST（随后吸收 seed 标量） | chunking.rs:2337 |
| `b"model_inputs"` / `b"model_chunks"` | 流式全局 transcript：输入绑定 / chunk 数 | verifier.rs:51/63 |
| chunk 结构吸收 | chunk_id、incoming/inputs/outgoing/outputs（字节级见 §5.1） | chunking.rs:511-550 |
| 承诺吸收 | witness → multiplicity → chunk IO（顺序固定） | lookup/context.rs:443-482 |
| `b"Internal round"` | sumcheck 每轮挑战（前置：u64 变量数头 + u64 max_degree 头 + 轮多项式） | sumcheck/prover.rs:44/207/437 |
| `b"batching_challenge"` | einsum 主 sumcheck | einsum/prove.rs:158 |
| `b"input_batching_challenge"` | einsum input aggregation | einsum/prove.rs:277 |
| `b"input_batching"` | pooling 层（×2 连续采样） | pooling.rs:507 |
| `b"batch_pooling"` | pooling 层 | pooling.rs:458 |
| `b"claims_batching"` | split 层 claim 合并 | split.rs:430 |
| `b"normalisation"` | 归一化变体查表层 | variant.rs:370 |
| `b"sum"` + 变体追加挑战 | linking sumcheck（sum 挑战 + 变体个数挑战） | inputs/proving.rs:44-49 |
| `b"table_constant"` | ChallengeStorage 常数挑战 | iop/mod.rs:176 |
| 表名（`GeLU`/`SiLU`/`ReLU`/`Requantise`/`ShiftCheck`/`SignedZero`/`Zero`） | 每张表的挑战派生（generate_challenge 内吸收表名） | lookup/table 定义处 |
| `b"initial_batching"` / `b"initial_alpha"` / `b"initial_lambda"` | LogUp 首轮三元挑战 | logup_gkr/prover.rs:171-173 |
| `b"logup_batching"` / `b"logup_alpha"` / `b"logup_lambda"` | LogUp 每轮三元挑战 | :284-289 |
| `b"combine subset evals"` | challenge 幂序列派生 | dp-crypto util.rs:207 |
| LogUp 实例输出 | 证明开始前吸收：实例数 + 各实例各列顶值 | logup_gkr/prover.rs:163-166 |
| batching sumcheck 的 claim evals | 每条 claim 的 eval 先吸（1×Fr/条） | poly_commit/prover.rs:170-172/199-201 |
| `b"model_batching"` | batching sumcheck 的挑战 | poly_commit/prover.rs:233 |
| RLC 系数 | 每张被引用表采样 1×Fr（先吸 sumcheck 点与 evals） | poly_commit/prover.rs:115-120 |
| HyperKZG 内部 | 折叠承诺 coms → 采 r；v 全部 → 采 q_powers；w → 采 d₀ | arkyper/mod.rs:371-373/:212-214/:230-231 |
| `b"arkyper-kzg"` / `b"batch_open"` | **验证侧**子 transcript（FPGA prover 不涉及，联调对拍时知晓即可） | arkyper/mod.rs:520/622/640 |

> 维护规则：新增任何吸收点，先在此表登记（含字节格式），再做硬件。

## 附录 B：代码位置索引

| 主题 | 位置 |
| --- | --- |
| prove_chunk 主体 | `zkml/src/iop/prover.rs:588` |
| 流式协调（FPGA 桥接入点） | `zkml/src/iop/streaming.rs:272-285` |
| witness 生成 / multiplicity / batch_commit 调用 | `zkml/src/lookup/context.rs:233 / :317 / :417` |
| EQ 表 evals | dp-crypto `poly/eq.rs:33-70` |
| fix_low 折叠 | dp-crypto `poly/dense.rs:454-502` |
| evaluate / evaluate_at_chi / eval_as_univariate | dp-crypto `poly/dense.rs:584 / :741 / :765` |
| linear_combination / as_view_chunks | dp-crypto `poly/dense.rs:934 / :885` |
| sumcheck 引擎（两阶段） | dp-crypto `sumcheck/prover.rs:116`（Phase1 :23-103） |
| 单项式展开宏 | dp-crypto `sumcheck_macro/src/lib.rs:62-175` |
| 重心外推 | dp-crypto `sumcheck/extrapolate.rs:1-60` |
| 线程数选择 | dp-crypto `sumcheck/util.rs:197` |
| transcript trait / Blake3 | dp-crypto `arkyper/transcript/mod.rs / blake3.rs` |
| HyperKZG（SRS/commit/open/verify） | dp-crypto `arkyper/mod.rs:61-135 / :457 / :329 / :386` |
| 无余式商多项式 | dp-crypto `arkyper/mod.rs:157-183` |
| batch_poly_msm（CPU/GPU） | dp-crypto `arkyper/msm.rs:41 / :60` |
| einsum fix_axes | `zkml/src/layers/einsum/axis.rs:478/503/1142` |
| batching sumcheck / RLC | `zkml/src/poly_commit/prover.rs:140 / :115` |
| LogUp 批量引擎 | `zkml/src/lookup/logup_gkr/prover.rs:91` |
| LogUp 电路 / 层表达式 / 终值合并 | `zkml/src/lookup/logup_gkr/circuit.rs:88-184 / :231-412 / :442-500`；输入构造 `structs.rs:140-272`；分数算术 `structs.rs:25-119` |
| Expression::Challenge 求值语义 | dp-crypto `sumcheck/expression/utils.rs:89-93`（`challenges[id]^pow·s+off`） |
| 卷积六段证明 | `zkml/src/layers/convolution/mod.rs:784-1120`（权重版 :698-779）；FFT 批量 sumcheck `iop/prover.rs:461-577`；委托链 `:328-382`；φ 递推 `:385-455` |
| linking sumcheck | `zkml/src/lookup/operation/inputs/proving.rs:26` |
| 查表层泛型证明 | `zkml/src/lookup/operation/generic_prove.rs:66` |
| 边界 claims | `zkml/src/iop/chunking.rs:717/746` |
| 流式验证重放 | `zkml/src/iop/verifier.rs:755`（绑定点 :47/:62/:72） |

## 附录 C：术语表

| 术语 | 含义 |
| --- | --- |
| Fr / canonical | BN254 标量域元素 / 其 32B 小端标准编码（非 Montgomery） |
| MLE / 表 | 多线性扩展的 2^n 求值表 |
| claim | 求值主张 f(point)=eval |
| EQ 表 | eq(r,·) 的 2^n 求值表 |
| fix_low | 表折叠：相邻对 (z[2i], z[2i+1]) → z[2i] + r·(z[2i+1]−z[2i]) |
| claimed sum | sumcheck 初始断言的求和值 Y |
| 虚拟多项式 | 多张表按单项式组合、一次 sumcheck 处理的机制 |
| MSM | 多标量乘 Σ sᵢ·Gᵢ；Pippenger = 桶分窗算法 |
| SRS / g1_powers | trusted setup 基点序列 (G, τG, τ²G, …) |
| RLC | 随机线性组合 Σ cᵢ·Pᵢ |
| LogUp | 基于分数 m/(β+v) 的查表论证 |
| multiplicity | 表位置的查值次数 ÷ 表内出现次数 |
| DST | transcript 构造时吸入的域分隔标签 |
| XOF | BLAKE3 可扩展输出流（挑战 = 其前 16 字节 mod p） |
