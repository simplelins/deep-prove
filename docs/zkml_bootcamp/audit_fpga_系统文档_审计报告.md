# DeepProve FPGA 系统文档 · 审计报告

> 审计对象：[deepprove_fpga_系统文档合并版.md](deepprove_fpga_系统文档合并版.md)
> 审计视角：零知识证明算法工程师 × FPGA 架构师 交叉
> 审计方式：逐关键参数与 `/home/lww/project/06_zkp/deep-prove` 源码核验，区分「已核实 / 估算 / 未验证」三档
> 审计日期：2026-09-20

---

## 结论速览（TL;DR）

| 维度 | 评级 | 一句话 |
|---|---|---|
| 算法理解深度 | ⚠️ 方向性错配 | 主线选错（BaseFold/Goldilocks 代码里没有），但 EQ/sumcheck/last-token 又理解得准 |
| 数据流清晰度 | ✅ 准确 | 流式 + chunk 乒乓 + 双通道握手，与代码吻合，是全文最有价值部分 |
| 性能瓶颈分析 | ⚠️ 半对 | 瓶颈=带宽判断正确；带宽数字按 8B 域宽算，实际应 32B，差 4× |
| 能否支撑 FPGA 设计 | ❌ 暂不能 | M1 规格与数据量/带宽/HBM 预算落在错误的域上，直接照做会翻车 |

**两个决定性硬事实（均已源码核验）**：

1. **实际证明域 = `ark_bn254::Fr`（254-bit，32 B/元素）**，全仓库无一例外：`lib.rs:306`、`iop/mod.rs:225`、`poly_commit/mod.rs:220`、`number.rs:228`、`bin/bench/llm.rs:37`。
2. **实际承诺方案 = `HyperKZG<Bn254>`**（CUDA 下 `HyperKZGGpu<Bn254>`）：`bin/bench/llm.rs:37-39`、`poly_commit/mod.rs:222`、`testing.rs:9`；`dp-crypto` 分支为 `feat/hkzg_gpu`。
   **仓库中不存在 BaseFold / Ceno / foldable code / Goldilocks**（`quantization/mod.rs:219` 仅注释提及这些域为 ToField 的可能目标，实际实例化永远为 `ark_bn254::Fr`）。

---

## 一、CRITICAL —— 主干路线建立在代码中不存在的域上

### 文档主张 vs 源码事实

文档多处主张（第1部分 §2.1、第2部分 §1.2/§6、里程碑 M0/M2、第5部分整章）：
> BaseFold（Ceno）+ Goldilocks 64 位 = 主线；HyperKZG/BN254 = 后置 M4。
> 并以「GPU 加速的正是 HKZG」作为旁证。

该旁证自相矛盾：GPU 版（`feat/hkzg_gpu`）加速的是 **HyperKZG**，而 HyperKZG 的域是 **BN254**，不是 Goldilocks。全文无任何代码路径能连到「推荐的 64 位 Goldilocks 主线」。

### 级联后果：不是一处笔误，而是 4× 放大的系统性偏差

文档几乎所有数据面数字按 **8 B/元素（Goldilocks）** 计算，实际按 **32 B/元素（BN254 Fr）** 应为：

| 数据面项 | 文档按 8B | 实际按 32B | 偏差 |
|---|---|---|---|
| 权重驻留 GPT-2 | ~2 GB | **~8 GB** | 4× |
| 权重驻留 Gemma-3 | ~4.3 GB | **~17 GB** | 4× |
| 每 chunk 读 | 8 MB | **32 MB** | 4× |
| 激活单张量 seq512 | ≤8 MB | ≤32 MB | 4× |
| 注意力 n=1024 全矩阵 | 96 MB | **384 MB** | 4× |
| 论文远峰（HKZG） | 147 GB（已列） | —（文档却按 GF 算 FPGA） | — |

**对 FPGA 落地影响**：
- **HBM 容量**：原稿「≥16 GB 权重常驻」对 BN254 权重（8–17 GB）余量从「宽裕」变「贴边/不够」。
- **M1 规格整体作废**：第5部分「64 位 Goldilocks MAC（1 DSP=1 乘法）+ EQ 引擎」在 254-bit 模乘下不成立——254-bit 模乘需 6–8 个 64-bit limb + Montgomery，面积/延迟是 4–8×；EQ 引擎每元素 8B→32B，URAM 占用同比涨 4×。
- **带宽预算 / DMA 突发 / URAM 缓冲**：全部需按 32B 重排。

**两条可走的路，二选一，不可默认第一个存在**：
- **(a) 正视现状走 HyperKZG/BN254 主线**：M1 直接做 254-bit 模乘 + 曲线 MSM（对照 `feat/hkzg_gpu`，含 Pi basis），数据面按 32B 重算。
- **(b) 若真要 Goldilocks**：明确标为「仓库尚不存在的改造」，把 `ToField`+`number.rs`+iop 全泛型的改动量列为 HIGH 风险，不能当成「已核实的主线」。

---

## 二、HIGH —— 时间占比（57%/35%/92%）来源错挂 + 精度归因存疑

- `57% 承诺 / 35% sumcheck / 92% 热块` 出自**外部 DeepProve 论文 Table 7/1**，**本仓库没有对应 profiling 代码**。文档多处标「已核实」，实际是「论文数字」。
- 且 92% 拆分是论文在**那个具体模型/域**上的结果。改走 BN254（域变 4× 宽）后，witness 承诺与 sumcheck 相对占比会位移，**不能直接作为「FPGA 先加速谁」的依据**。M2/M3 排序需要一份**用真实域的实测基准**。

---

## 三、HIGH —— 「已核实」与「估算」的界限失守（注意力裁剪是最大疑点）

- 第4部分 §5.2 / 表5.1 / 风险表断言：**注意力掩码可裁剪到 ~n²/2 或「认证行」**，据此把二次项说成「带宽黑洞，可裁剪」。
- 真实代码确实**只证明最后 token**（`llm.rs:537` `last_token_tensor = ...select(0,index)`），但对 **`[n,n]` 注意力 score 矩阵本身的承诺是否可裁、能裁到几行**，文档无代码依据，也**未标注为「假设」**——却写进了结论表与风险缓解栏。
- 这直接影响「带宽黑洞是否真的存在」这一核心判断。需到 `exec_graph`/sumcheck 路径求证是「真承诺矩阵」还是「动态求值」。
- 另：Gemma-3 vocab=262144 标「已核实」，引用的是 **gguf** shape；本项目解析路径是 safetensors/BF16，域与格式与 gguf 不一致，文档未说明该差异。

---

## 四、MED —— 数据流与架构：方向对，但需按真实后端重新对齐

**正确的部分（应保留）**：
- chunk(2²⁰)/MAX_CHUNKS=128、pad 到 2 幂、`to_field_mle` 泛型、`VirtualPolynomialsBuilder`+`eq_polys` 批量 sumcheck、EQ 张量分解 DP、逐轮 random→握手、不能脱机批处理、权重预载常驻、验证侧免 FPGA —— **全部源码核验属实**。
- 瓶颈判定「C0/C1 每 chunk 读大、写 <1KB，纯读取带宽受限 + HBM 刚需」—— **方法正确**，仅字节数需 8B→32B。

**需重新对齐的部分**：
- 「FPGA-A 做承诺」原稿写作「折叠码编码+哈希」，真实 HyperKZG 的承诺负载是 **BN254 曲线 MSM（多标量乘）**，完全不同。
- 文档把「打开/求值」标 **1%**（第2部分 §1.1）：在 HyperKZG 里打开/多点求值是**另一组 MSM**，与承诺**同硬件引擎**——打开会从「可忽略」抬成「不可忽略的共享负载」，文档低估了。

---

## 五、LOW / 正确且建议保留

- 第4部分「上下文 n～256–512 最优窗口」「n 只影响动态路径、权重静态」—— **正确**。
- 证明时间「64→512 只涨 5.5×（亚线性），因只证明末 token」—— **与代码 `select(0,index)` 吻合，正确**。
- 验证时间/证明大小与 n 解耦—— **正确**。

---

## 六、修正优先级

| 优先级 | 项 | 动作 |
|---|---|---|
| **P0** | 主线判定 | 改判为 **HyperKZG/BN254**，或把 Goldilocks 明示为「待改造大前提」 |
| **P0** | 数据面 | 全部 8B → **32B 重算**（权重/HBM/带宽/DMA/URAM） |
| **P0** | M1 规格 | 改为 **254-bit 模乘器 + MSM 承诺原语**（对标 `feat/hkzg_gpu`），弃 64-bit Goldilocks MAC |
| **P1** | 时间占比 | 57%/35% 标注为「论文数字」，补真实域 profiling 作 FPGA 优先级依据 |
| **P1** | 注意力裁剪 | 标注为「假设」，求证矩阵承诺是否可裁、能裁到几行 |
| **P2** | 打开/求值 | 在 HyperKZG 下重估「打开」负载（与承诺同源 MSM） |

---

## 七、对三个核心问题的直接回答

1. **算法理解是否透彻？** 对 EQ/sumcheck/last-token/上下文那一半透；但对**本项目的域与承诺原语**这一半，主导成了「论文描述的 BaseFold」而非「代码实现的 HyperKZG」——把**别人的论文基线**当成了**自己的现状**。
2. **数据流是否清晰？** 清晰且正确，是文档最优部分。
3. **能否支撑 FPGA 设计？** **暂不能直接照做**。照第5部分 M1 搭 64-bit Goldilocks MAC 会在错误的硬件上浪费一个完整里程碑。先改主线为 HyperKZG/BN254、数据面重算到 32B，文档才能作为 FPGA 设计的有效输入。

---

## 八、（追加）处置状态 2026-09-20

上述问题已对照 [deepprove_fpga_系统文档合并版.md](deepprove_fpga_系统文档合并版.md) **全部修正完毕**：

| 审计优先级 | 项 | 处置 |
|---|---|---|
| **P0** | 主线判定 | ✅ 全文主线改判为 **HyperKZG/BN254**；Goldilocks 统一定位为"理论对照/未实现 + 若要想=ToField 全泛型化改造（HIGH）" |
| **P0** | 数据面 8B→32B | ✅ 决定层/瓶颈/M1/权重驻留/带宽/chunk 均按 **32 B** 重排（GPT-2 权重 8 GB、Gemma 17 GB、每 chunk 32 MB、注意力 n=1024 384 MB） |
| **P0** | M1 规格 | ✅ 改为 **254-bit Montgomery 模乘 + BN254 EC/MSM 承诺原语**（对标 `HyperKZGGpu`）；弃 64-bit Goldilocks MAC（保留为"理论对照"） |
| **P1** | 时间占比 | ✅ 57%/35%/92% 全标注为**论文数字（Table 7），非本仓库实测**，并注明需真实域 benchmark 定 M2/M3 优先级 |
| **P1** | 注意力裁剪 | ✅ 标注为**假设/待求证**；补充已核实证据（`llm.rs:537` 只证末 token；`attention_mask/prove.rs` 含 `fix_lower_mle_rows/columns` 下三角 MLE），上板前须求证矩阵是否整体物化承诺 |
| **P2** | 打开/求值 | ✅ 明确 HyperKZG 下打开与承诺**同源 MSM、共享引擎**，并单独排负载预算（非 1%） |

> 文档顶部已加**审计修正声明**（第1部分之前），全文以 BN254/32B 主线为准；所有旧版错误表述均以"⚠️ 已勘正"给出回溯。

**任务 B（§9）处置（2026-09-20）**：对照新增 §9，已把「打开/求值」从"**1%**"修正为"**与承诺同源 Pippenger MSM、G1 负载约 4× 承诺、须独立预算**"。全文落点均已对齐——顶部声明新增第⑤条、§2.1/§2.3 表注、Part 1 架构图 FPGA-A 标注、风险表、Part 2 §1.1/§6.1、Part 3 §4(C3)、**Part 5 新增 §9「HyperKZG 承诺/打开算子结构与负载修正（源码核定）」**（即本文 §9 的系统文档版，含 9.1–9.7）。核心红线贯穿：**FPGA-A（MSM 引擎）必须同时覆盖承诺+打开，是同一块硬件；M2 验收按"承诺+打开"合并计量，不能只按承诺 ~57%。**

---

## 九、（追加）任务 B：HyperKZG 承诺/打开确切算子结构与负载修正（2026-09-20）

> 依据：`dp-crypto`（`feat/hkzg_gpu`，锁定 commit `22e8e93`），源码路径
> `/home/lww/usr/.cargo/git/checkouts/dp-crypto-8724f154af4b9820/22e8e93/dp-crypto`。
> 以下算子、复杂度与负载系数全部从源码核定；**时序数值在源码中未写死**（皆为 `examples/hyperkzg_gpu.rs` 运行时打印 / divan bench），凡引数字均为"量级估"，非源码常量。

### 9.1 域与表示（FPGA 硬件基线）
- 域：`ark_bn254::Fr = Fp256<MontBackend<FrConfig,4>>` —— **254-bit，4×u64 limb，全程 Montgomery 表示**。基域 `Fq` 同构（曲线坐标）。
- 序列化：**字段元素 32 B**；`G1Affine` 36 B 压缩 / 64 B 非压缩。
- G1 承诺 = 1 个 32 B（压缩）。**这是 M1 域乘法必须面对的 254-bit Montgomery 模乘，非 64-bit Goldilocks。**

### 9.2 承诺（commit / batch_commit）—— 本质是一个 Pippenger MSM
- 单多项式 `commit`：`msm::poly_msm(g1_powers, poly)` → `VariableBaseMSM::msm`，即 **1 个 size=n 的 Pippenger 多标量乘**（标量 = 该多项式的 n 个域求值，基点 = SRS 的 G1 powers，长度与多项式等长 n）。
- `batch_commit`：P 个多项式 → **P 个独立 MSM**，各 size=nᵢ。CUDA 下基点表只上传一次、按 `max_len` 预转换后逐多项式切用（`gpu_batch_commit`）。
- 输出：每个多项式 1 个 G1。`OpeningProofHint = ()`，**本 crate 无 open 加速 hint**（不像 Dory）。
- **负载**：witness 承诺热块 = 逐个张量做 Pippenger MSM，size 到 n=2²⁰（chunk）。这是纯"G1 加法主导"的密集椭圆曲线算术，**不是"折叠码编码+哈希"**——与原文档 §2.1 描述完全不同。

### 9.3 打开（open）—— 原文档标"1%"是错的，实际是承诺同源、且 MSM 负载约为承诺的 4×
打开一个 k-元、长 n=2^k 的 MLE 分四步（`open()`，`mod.rs:329-383`）：

| 步骤 | 算子 | 复杂度 | FPGA 性质 |
|---|---|---|---|
| ① 超立方折叠(Phase1) | 逐层 `fix_low`：`j → r_i*(f[2j+1]-f[2j])+f[2j]`，累计 k−1 轮 | **≈ n 域乘 + 2n 域加** | 数据并行、SIMD/脉动友好 |
| ② 中间承诺 MSM | `batch_poly_msm(polys[1..])`：长度 n/2,n/4,…, 2 各一个 | **Σ 尺寸 ≈ n−2 点乘** | 同承诺引擎 |
| ③ KZG 批打开 | 先 `b_poly = Σ q_powers·polys`（k× 线性组合），再 `compute_witness_polynomial`（3 次，每次 O(n)），再 **3 个 size=n 的 MSM** | **3·n 点乘（全 prover 最大 MSM 块）** | 同承诺引擎 + 长串行除法链 |
| （结果） | 产 k−1 个 G1 + 3 个 G1 + 3k 域 evals | — | — |

- **本 prover 每次打开的 G1 点乘总量 ≈ (n−2) + 3n ≈ 4n**，其中 **3n 来自阶段③的 witness MSM**；加上单独承诺的 n，**打开侧是承诺侧的 ~4× 量级**。
- `compute_witness_polynomial`（`mod.rs:171-183`）是**串行依赖链** `h[i-1]=f[i]+h[i]*u`（Horner 式合成除法，O(n) 一次乘加），**对 FPGA 流水/时序不友好**，需按窗口/SIMD 分解。
- 阶段①⑦的折叠是 `out[j]=r*(poly[2j+1]−poly[2j])+poly[2j]`，与 M1 的 EQ/fold 引擎同构，可复用。

### 9.4 GPU 蓝图（FPGA 应镜像什么）
`hyperkzg_gpu.rs` + `gpu_msm.rs` 明确把**三个 prover 阶段**都搬上 GPU，且全部是我们上面锁定的算子：
1. **折叠** `fix_var` / `fix_vars_with_intermediates`（GPU `poly_ops` 核，公式 `out[j]=r*(poly[2j+1]-poly[2j])+poly[2j]`）；
2. **中间承诺 MSM** `gpu_batch_commit`（`MultiexpKernel::multiexp`，即 **Pippenger**）；基点 Montgomery limb 上传、`Fr::into_bigint` 缩放；
3. **witness** `witness_poly` 核（`gpu_msm.rs:157-161`）。
→ **FPGA 加速器的"元内核集合"= 254-bit Montgomery Fr/Fq 模乘单元 + Pippenger MSM（bucket/window 法）+ 折叠/线性组合 SIMD 数组。** 这与原文档"64-bit Goldilocks MAC"完全是两套硬件。

### 9.5 验证侧（FPGA 无需做，但确认轻）
`kzg_verify_batch`：**1 个 size=(k+4) 的小 MSM** + **1 个 2-配对 pairing 检查**（`multi_pairing`）。验证者不是 MSM 重负载，**"验证侧无 FPGA 必要"结论成立**。

### 9.6 修正后的负载/瓶颈表（替代原 §2.3 的"打开 1%"）
| 热块 | 真实算子（HyperKZG/BN254） | 量级(每张量 n=2^k) | 原文档 | 修正 |
|---|---|---|---|---|
| witness 承诺 | Pippenger MSM ×P，size n | G1 点乘 O(n)·P | ~57% | ✅ 保留为热块，但实现改为 **MSM**（非折叠码） |
| sumcheck | 多轮 EQ/fold + 归约 | 域乘加 O(n) | ~35% | ✅ 保留 |
| **打开/求值** | **①折叠 O(n) + ②Σ≈n MSM + ③3n witness MSM** | **G1 点乘 ≈4n + 域乘 O(n)** | **∘"1%"** | ❌ **修正为与承诺同源 MSM，量级 ~4× 承诺，须独立预算（至少数%~十几%量级）** |

> **结论红线**：原文档"打开/求值 1%"严重低估；HyperKZG 下打开复用与承诺完全相同的 **Pippenger MSM 引擎**，其 G1 负载约为承诺的 4×。**FPGA-A（承诺器）必须同时覆盖打开——两者是同一块硬件。** M2 的验收基准应把"承诺+打开"合并计量，不能只按承诺的 ~57%（且该 57% 是论文数字，真实占比需按 9.7 实测）。

### 9.7 待实测项（源码无此常量，需跑 benchmark 定）
- `examples/hyperkzg_gpu.rs` 运行时会打印 CPU 全路径 vs GPU 补的 commit/batch/prove **Speedup**，但源码不写死数值 → **真实加速比/占比须在目标硬件实测**。
- 测试/bench 覆盖长度：commit/open `n=2^16…2^23`、dense `2^12…2^24`、sumcheck `num_vars=18…26`、correctness `ell∈{8..16}` → **FPGA 必须覆盖 n=2^8…2^26**；SRS `g1_powers` 表长 ≤ max_degree = 2^max_num_vars（`g1_powers` 每 G1 64B 非压缩 / 32B 压缩，大 n 时基点表数百 MB 级，宜流式或分片驻留 HBM）。

---

## 十、（追加）FPGA 设计就绪度评估 + 真实 benchmark 计划（2026-09-20）

> 触发：用户问「当前文档是否对项目进行了充分评估，可以支撑开展 fpga 设计」。本节给**三档结论**：已充分的部分 / 决定性缺口 / 如何用一次真实 benchmark 补上。
> 原则：本节**不直接改动系统文档**，作为审计报告的独立评估结论输出。

### 10.1 总体结论：方向已就绪，但不等于可直接开板

对「能否支撑开展 FPGA 设计」逐层回答：

| 层 | 是否充分 | 依据 |
|---|---|---|
| 可行性 / 架构评审 | ✅ **充分** | 主线 HyperKZG/BN254、32B 数据面、带宽瓶颈、chunk 乒乓、MSM=承诺+打开共享引擎、权重常驻 8–17 GB —— 全部源码核验，方向正确，架构图可按此评审 |
| **M1 原语落地** | ❌ **不充分（先决缺口）** | 254-bit Montgomery 模乘器、曲线 G1 add/double、Pippenger MSM 的 **window/bucket 参数与吞吐/面积预算**在文档里只有方案级描述，无可实现的微架构规格 |
| **性能优先级** | ❌ **不充分（实测缺口）** | 57%/35%/92% 是论文数字，open≈4×commit 是**单算子比例**，不是系统级真实占比；M2/M3 先做谁、chip2 块额分配，缺一份**用真实域实测**的基准 |
| 时序/精度目标 | ❌ 未定 | 目标板卡、时钟、HBM/URAM 预算、端到端延迟指标未定 |

**一句话**：文档已足以支撑「该不该上 FPGA、加速器长什么样、硬件共享与数据面」，但**不支撑直接 RTL**——缺少 M1 原语的微架构规格，且性能结论建立在论文数字上。

### 10.2 四个决定性缺口（按堵路程度排序）

1. **M1 原子算子规格缺失（P0，直接堵 RTL）** —— 需要写死：
   - 254-bit Montgomery 模乘的乘加结构（CIOS/FIPS 变体、单周期吞吐 vs 面积）、`Fr`/`Fq` 两套常数。
   - Pippenger MSM 的 **window 宽 w、bucket 数量 2^w、桶累加器排布**；`compute_witness_polynomial` 的 Horner 串行链（`mod.rs:171-183`）如何按窗口/粒度 SIMD 化。
   - G1 点加/倍加的模逆流水（Jacobian 但需反模到仿射时的 2 次逆）在 URAM/DSP/时序预算内。
2. **真实 benchmark 缺失（P0）** —— 见 §10.3，这是把「论文占比」换成「本项目占比」的唯一手段。
3. **注意力/查找表细节（P1）** —— `fix_lower_mle_rows/columns` 下三角 MLE 是否整体物化承诺、vocab 262144 的 softmax 查找表做法未定。
4. **时序与精度目标（P2）** —— 板卡选型、时钟、HBM2e 带宽模型化、延迟/吞吐 SLA 未立。

### 10.3 真实 benchmark 计划（补上「决定性缺口 2」）

**目标**：用真实 BN254/HyperKZG 把「weight 承诺 / 每层 sumcheck / commit+open / inference」各自占比测出来，替换论文 57%/35%/92%，据此定 M2/M3 优先级与 chip2 分配。

**跑哪条命令**（CPU 基线，确认钩子齐备）：
```bash
cd zkml
# 单序列做端到端（含 quant/context/inference/proof 四段，输出 bench.csv）
cargo run --release --bin bench-llm -- \
  --model gpt2 --hf openai-community/gpt2 --sequence 64,128,256,512 --bench bench.csv

# 分层归因：--measure-layerwise 输出到 bench_layerwise.csv
cargo run --release --bin bench-llm -- \
  --model gpt2 --hf openai-community/gpt2 --sequence 64 --measure-layerwise

# 底层算子级：直接析出 commit/open/sumcheck/prove 各段，见下方解析
```
> 注：`bench-llm` 现已内置测量钩子：`HEADER_QUANTIZATION_TIME / HEADER_INFERENCE_TIME / HEADER_PROOF_SIZE`（`bin/bench/llm.rs:145-151`）、`--measure-layerwise`（`:141`）、按层计时的 `LAYER_WISE_MEASURE_PREFIX`。prover 侧已埋 `prove_commitment_opening`（`iop/prover.rs:836`）、`prove_claims`/`prove_full`（`:831 / :1027`）。**缺的是「weight 一次性 batch_commit」计时、以及 open 内部 4n 分解的单独计时** —— 这两处若需精确占比，需临时插桩（见下）分层。

**怎么从 bench.csv 解析占比**（bench.csv 一行一模型一测次）：
- `quantization_time` → weight 承诺前预处理（IntoProvableLlm 的 batch_commit 在其内）。
- `inference_time` → 很轻（只证明、先量化为 proof 的一部分）。
- `proof_size` → 不是时间，仅验证端。
- `proof_full`（`prove_full`）总时长 − `prove_claims` − `prove_commitment_opening` → sumcheck 净占比。

**临时插桩建议**（一次性，拿到数后可撤，属任务类改动非文档）：
- `context.rs:210` `model_commitments = ...batch_commit(...)` 外包 `measure::r("weight_batch_commit", ...)` → 得 weight 一次性承诺绝对时长（对 M1/HBM 有决定性意义：它主导 boot/persist vs 每 token 增量）。
- `prover.rs:836` 内 `commit_prover.prove` 退货部 `open` 内四阶段的 `measure`，得到 ①fold+②ΠMSM+③witness 3n MSM 各自占比，验证「open≈4×commit」是否在真实 n=2^16…2^22 上成立。

**这一测能定下的 FPGA 决策**：
1. M2/M3 排序：真实占比 vs 论文 57%/35% 是否位移 → 决定先加速 sumcheck 还是先做 MSM 引擎。
2. chip2 ÷ 承诺引擎 / 打开引擎的配比：4× 系数若实测成立，打开侧必须留足硬件。
3. 权重 batch_commit 一次性时间 vs 每 token 增量：决定加速器是「常驻权重、流式逐 token」还是「boot 时全量承诺」。

### 10.4 评估落点

补齐 §10.3（一次真实 benchmark）+ §10.2 缺口 1（M1 微架构规格，choose 一张板卡写死资源预算）两项后，文档即可从「可行性可评审」升级为「可直接支撑 fpga 设计」。在此之前：**不宜按论文占比直接排期 M2/M3**。
