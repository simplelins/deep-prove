# HANDOFF · 段证明 FPGA Demo 交接（2026-11-17）

> 目标读者：在另一台机器（Alveo U250 + Ubuntu 22.04 + Vivado/Vitis + RTX 3090）上
> 开启 FPGA demo 的新会话。本文只做**交接**：现状快照、代码地图、算力画像、demo 方案、
> 工作量评估。算法细节不重复，引用已有文档（§2 文档索引）。

---

## 0 一页结论

1. **段证明（prove_chunk）适合 FPGA demo，且有成熟规格**：已有算子规格书
   `prove_pipeline_FPGA算子开发指导.md`（OP-01~OP-14）与实测瓶颈定位
   `N_证明性能瓶颈分析与CPU_FPGA优化路线.md`。**P0-A = OP-07 sumcheck 轮引擎（linking 形态）
   + OP-13 LogUp GKR 轮引擎**——实测查表族占段证明算力 **82%**，MSM 仅 6%（原规格书的
   MSM 优先级已被实测反转）。
2. **合并为单个 Proof 不需要 FPGA**：`From<Vec<ChunkProof>> for Proof` = `Self { chunk_proofs }`
   纯结构拼接（iop/mod.rs:118）；逐 chunk 的 seed 链吸收 = blake3 哈希（毫秒级）。
   真正的重计算全部在**每段证明内部**（§4 算力画像）。
3. **CPU 侧 e2e 现状**：证明阶段已全通（gemma-270m s=1024 --alpha-swa，662 节点 0 失败，
   两次复现）；验证阶段剩 1 个已知问题（chunk idx 2 的 round-0 一致性，§3.3）——
   不阻塞 FPGA demo 启动（demo 对拍用的是 prove 侧输出）。
4. **demo 优先路径**：OP-01 Fr 域乘流水线 → OP-07/OP-13 轮引擎核心（eq-加权 MLE 求值 MAC）
   → 接入 prove_task 替换一轮 sumcheck 对拍。三方对拍：CPU(Rust) → 3090(CUDA) → U250(XRT)。

---

## 1 任务与环境

| 项 | 值 |
| --- | --- |
| FPGA | Xilinx Alveo U250（VU9P-class：~6.8k DSP48E2、~2.3M LUT、HBM2） |
| OS | Ubuntu 22.04 |
| 工具 | Vivado / Vitis（已装）+ XRT（注意与内核版本匹配，`xbutil query` 先验证） |
| GPU | RTX 3090——**参考实现/对拍**（CUDA BN254 Fr Montgomery + sumcheck dot），非证明主力 |
| 代码 | deep-prove 仓库，分支 `feat/streaming-trace-prove`，HEAD `0a7b1973` |
| 测试模型 | gemma-270m（MQA：6 heads/1 kv、head_dim 256、hidden 640、MLP intermediate 2048） |

---
### 1.1 模型与数据文件清单（scp 直接迁移，避免新机联网下载）

模型缓存 = **相对 CWD 的 `model_cache/` 目录**（`parser/mod.rs:157`）：
文件在缓存中即**离线加载，不触发 HF 下载**。e2e 命令从 `zkml/` 目录启动，
因此新机必须保持 `zkml/model_cache/<repo>/...` 的相对布局。

源机绝对路径前缀：`/home/lww/project/06_zkp/deep-prove/zkml/model_cache/`

**必需集（e2e 立即可用，约 550MB）：**

| 文件/目录 | 大小 | 用途 |
| --- | --- | --- |
| `google/gemma-3-270m-it/`（整目录：model.safetensors 511MB + config.json + tokenizer.* 等 10 个文件） | 549M | **e2e 测试模型**（gemma3, MQA） |
| `long_prompt.txt` | 160K | e2e 提示词（1024 token 输入） |

**可选集（扩展测试用）：**

| 文件/目录 | 大小 | 状态 |
| --- | --- | --- |
| `openai-community/gpt2/` | 526M | 完整（CPU/小规模对拍友好） |
| `google/gemma-3-1b-it/` | 1.9G | 完整 |
| `meta-llama/Llama-2-7b-hf/` | 26G | 完整（N 文档实测用过；大，按需） |
| `meta-llama/Llama-2-13b-hf/` | 25G | 完整（大，按需） |
| `LLM-Research/Llama-3.2-1B/` | 16K | **不完整**（safetensors 仅 4KB stub），勿用 |

**scp 命令（源机 → 新机，保持相对布局）：**

```bash
# 新机上先建好目录结构：<work>/zkml/model_cache/
# 必需集（~550MB，几分钟）
scp -r lww@<源机>:/home/lww/project/06_zkp/deep-prove/zkml/model_cache/google \
      <work>/zkml/model_cache/
scp lww@<源机>:/home/lww/project/06_zkp/deep-prove/zkml/model_cache/long_prompt.txt \
      <work>/zkml/model_cache/

# 可选：gpt2（小模型对拍）
scp -r lww@<源机>:/home/lww/project/06_zkp/deep-prove/zkml/model_cache/openai-community \
      <work>/zkml/model_cache/
```

**e2e 启动命令（新机 `<work>/zkml/` 目录下执行；bench-llm 需先 cargo build）：**

```bash
RUST_LOG=info ../target/release/bench-llm \
  --model gemma3 --hf google/gemma-3-270m-it --sequence 1024 \
  --streaming --stream-budget-mb 512 --stream-window 2 --alpha-swa \
  --prompt-file model_cache/long_prompt.txt \
  --bench model_cache/results/alpha_e2e/g270m_s1024.csv \
  > model_cache/results/alpha_e2e_v<N>.log 2>&1
```

注意：
- 路径全部相对 CWD（`../target/...` 与 `model_cache/...`），目录布局错了会静默走
  网络下载或找不到文件
- `model_cache/results/`（bench CSV 输出目录）按需 mkdir
- 不要迁移本机的 `target/`（新机重编译；旧二进制含本机路径的调试 dump 逻辑）

---

## 2 必读文档索引（不要重复造轮子）

| 文档 | 内容 | 对 demo 的用处 |
| --- | --- | --- |
| `prove_pipeline_FPGA算子开发指导.md` | **OP-01~OP-14 算子规格书**（v1.1），含接口/位宽/优先级 | **demo 的主规格书**，直接按 OP-01/07/13 开工 |
| `N_证明性能瓶颈分析与CPU_FPGA优化路线.md` | 实测瓶颈（查表族 82%）、每段 110s 分解、10 分钟达标算术 | 优先级依据；内存硬边界（s×模型上限） |
| `J_流式Trace证明机制.md` | 流式分段机制、FPGA bridge 的段级契约 | bridge 插入点的协议侧描述 |
| `K_流式证明协议审计与挑战值派生.md` | seed 链协议（plan A）、挑战派生顺序 | **挑战派生必须留 CPU**，FPGA 只做纯函数计算 |
| `O_ASIC架构研究路线_13b_32k_1分钟.md` | ASIC 远期路线 | 背景参考 |
| `L_长上下文路线图_SWA与流式证明.md` | D1.1 方案 α（banded attention） | 新算子 GatherRows/ScoreWindow 的来源 |

---

## 3 当前系统状态快照（截至 2026-11-17，HEAD 0a7b1973）

### 3.1 e2e 状态（gemma-270m s=1024 --alpha-swa，46 chunks / 662 节点）

- **证明阶段：全通**。v14、v15、v16 三次运行均 662 节点 0 失败（每轮 ~2h CPU，128 核打满）。
- **验证阶段：过半**。此前 3 个验证侧 bug 已修（见 §3.2 #7/#8/#9）；当前失败点：
  chunk idx 2 (Chunk(43)，首个注意力段) 的一个 **17 轮 multi-size lookup GKR**
  （wit_evals(16)，查表族形态）round-0 一致性失败——本会话末尾的现象：
  `0th round's prover message is not consistent with the claim. 1053672714079132685955910354650...`
  —— **CPU 侧下一个会话的第一件事**（诊断手法：verify 侧 round-0 的 wit_evals/claim
  dump 已就位，沿用 prove/verify 双侧 [gkr-round]/[gkr-verify] 对比法）。
- 注意：证明阶段全通意味着 **prove 侧输出（ChunkProof）是完整可序列化的**——
  FPGA demo 的对拍数据源已就绪，不等 CPU 侧验证收尾。

### 3.2 本会话修复的 9 个 bug（均有 panic/错误实锤，详见对应 commit）

| commit | bug | 对 demo 的启示 |
| --- | --- | --- |
| 79d0f1e9 | GKR 挑战推导改独立 transcript 双侧镜像 | 挑战链对状态极敏感 |
| 02b25199 | prove_gather 重复创建 transcript 抹掉 cc/csc absorb | 同上；transcript 操作序列必须逐操作镜像 |
| b107098b | gather input_claims 误用 lookup 点（27 维 vs 18 变量） | claim 点长度 = poly 变量数，段归属别混 |
| 3d23cb16 | AttentionMask 矩形网格 [h,q,w] 支持 | α 新算子，规格书里没有 |
| ecd30877 | SW gather clamp 三处统一为 unpadded s | padded/unpadded 双轨教训 |
| 02993cd3 | PCS RLC sumcheck max_num_vars 覆盖 eq 多项式 | 点长可超 poly 自然变量数（0-padding 机制） |
| 324e4172 | verifier 补 GatherRows dispatch（write+verify_chunk） | 新算子要补全**所有** dispatch 点 |
| 96bbb6b8 | mask row lt **cycle bug**（行模式写成列模式）+ 位序实证 | **§3.4 的位序约定** |
| 0a7b1973 | verify_gather 表索引闭式改 rows_pad——修复裸数字 panic "1023" | **closed-form 向量长度必须 = padded 网格** |

### 3.3 位序约定（FPGA kernel 必须遵守，已用探针测试实证并留档 lib.rs）

- **DensePolynomial 变量 0 ↔ 索引 LSB**（探针：δ_{index=1} 的 evaluate = r0·(1−r1)）
- **dp-crypto `eq::evals(r)`：r[0] ↔ 索引 bit 0**（LSB-first）
- MLE evaluate = Σ_i eq(r,i)·z[i]，manual dot 与 evaluate 逐位一致（探针 `test_dense_var_order_probe`）
- 因此：**flat = row·cols + col 的网格 → 变量 [0..cv) = col 位、[cv..) = row 位；
  sumcheck_point 前 cv 个元素 = col 的挑战**
- FPGA 侧 eq 表生成/MLE 求值的索引方向若与 CPU 不一致，对拍立刻失败——先跑
  `cargo test --lib eq_convention_probe test_dense_var_order_probe` 留意约定，再写 RTL

### 3.4 padded/unpadded 双轨（本会话 3 个 bug 同族）

- 所有表/closed-form 的**长度一律用 padded 网格**（rows_pad/cols_pad，2 的幂）；
  unpadded 只做 clamp 语义（gather 的源行钳位）
- 非 2 幂长度进 `into_mle` 会裸数字 panic（`sumcheck/mod.rs:32`，payload = 长度值，
  e2e 实锤 "1023"）
- prover 与 verifier 的同一 closed-form 向量必须同一来源（一个用 rows_pad 一个用
  unpadded 就会 round-0 失败）

---

## 4 段证明代码地图与数据流（FPGA bridge 的插入点）

### 4.1 全流程

```
run loop（生成 trace）
  └─ StreamingProver: 按 512MB 预算切 46 chunks
       ├─ 每 chunk 完成 → ProveTask { chunk_id, ModelChunk, chunk_trace, chunk_layers } + seed
       │    └─ resident prover thread（streaming.rs pipeline_prover_thread）
       │         └─ prove_task → Prover::prove_chunk（iop/prover.rs:588）
       │              ├─ transcript 初始化（seed 派生，K 文档 §5.2）
       │              ├─ witness 实例化 + 承诺（fieldize i64→Fr → MLE → HyperKZG batch_commit）
       │              ├─ claims 生成（输出/边界点随机求值）
       │              └─ backward_iter 逐节点 prove：
       │                   ├─ einsum → layers/einsum/prove.rs prove_internal（主 sumcheck）
       │                   ├─ 查表族（requant/rmsnorm/softmax/logits）
       │                   │    ├─ linking sumcheck（operation/inputs/proving.rs prove_linking_sumcheck）
       │                   │    └─ LogUp GKR（lookup/logup_gkr/prover.rs new_batch_multiple_sizes_prove）
       │                   ├─ gather（D1.1 α 新算子）→ gather_rows/prove.rs prove_gather/prove_score_window
       │                   ├─ attention mask → attention_mask/prove.rs prove_internal
       │                   └─ add/positional/embeddings…
       ├─ seed 链吸收：bind_proven_chunk_to_transcript（结构+承诺的 blake3）→ 下一 seed
       └─ finish()：merge（纯拼接，§5）+ trace.to_verifier_io
```

### 4.2 FPGA bridge 的契约边界（streaming.rs 原注释）

> "This thread is also the natural place where an FPGA/ASIC bridge will plug in:
> it receives `(chunk structure, sub-trace tensors, seed)` per segment — all
> serializable — and returns serialized `ChunkProofs`."

即 **段级替换** 是第一类支持形态：整段 prove_task 下沉 FPGA。但 demo 不建议一上来
就段级下沉——先做**算子级替换**（见 §6 里程碑）。

### 4.3 关键数据结构（host↔FPGA 传输对象）

| 对象 | 内容 | 规模（gemma-270m s=1024 实测） |
| --- | --- | --- |
| `DensePolynomial`（MLE） | `Vec<Fr>`（BN254 Fr，Montgomery 4×64 limb） | 最大 witness：V-gather K' = 2^19×256 = 2^27 elem（4GB） |
| sumcheck 轮消息 | 每轮 max_degree+1 个 Fr | 小（几十字节） |
| eq 表 | `evals(point)` 2^n | n ≤ 21（demo 建议规模） |
| LogUp 折叠 | 分数 (n,d) 对 | 同 witness 规模 |
| 挑战 | blake3 XOF 128-bit | ——（**派生留 CPU**） |

---

## 5 「合并为单个 Proof」工作量评估（问题 2 的答案）

### 结论：**不需要 FPGA，也基本不需要开发**

1. **merge 本体 = 纯结构拼接**：`iop/mod.rs:118`
   `From<Vec<ChunkProof>> for Proof` = `Self { chunk_proofs }`——零密码计算。
2. **协议级"合并"= seed 链**：每段证明完成后
   `bind_proven_chunk_to_transcript`（chunk 结构 + IO 承诺的 blake3 吸收）+ 一次
   `challenge_scalar`——每段毫秒级，46 段总计 < 0.1s，纯 CPU 串行（协议要求顺序，
   见 K 文档——这里**不能也不需要并行**）。
3. **N 文档实测**：finish 阶段的 1462s（270m@4k）是**等最后一段（logits 超尺寸段）
   证明完成的排空延迟**，合并本身可忽略；verify 全程 8~84s（证明时长的 1-2%）。
4. **验证侧无需 FPGA**：sumcheck 验证 O(log)、每段 2 次 pairing、MSM 仅小规模
    opening 核对——CPU 已足够。

**资源建议**：把 u250 的全部资源留给段证明的 OP-01/07/13；3090 做 host 侧
预/后处理加速（如 i8 推理前向，N 文档 §0.2）属于另一条线，demo 阶段只当对拍器。

---

## 6 FPGA demo 方案（对齐规格书 P0-A）

### 6.1 优先级（引用 N 文档实测：查表族 82% / einsum 8% / MSM 6%）

| 序 | kernel | 规格 | 为什么 |
| --- | --- | --- | --- |
| 1 | **OP-01** Fr 域乘/加/减流水线 | BN254 Fr，Montgomery 4×64 limb；目标每周期 1 乘 | 全部算子的地基；DSP ~24-36 个/乘法器，流水化 |
| 2 | **OP-05/07 合体** eq-加权 MLE 求值 MAC：输入 eq 表 + k 条 witness MLE 流，输出 Σ eq[i]·Π MLE_j[i]（sumcheck 单轮的核心） | 流式，HBM 带宽约束 | **linking 形态 = 82% 算力的主体**；纯乘加、无分支，最适合 FPGA |
| 3 | **OP-13** LogUp GKR 轮引擎（分数折叠 n1·d2+n2·d1） | 与 OP-07 同引擎合并实现（规格书建议） | 查表族第二大头 |
| 4 | OP-06 fix_low 折叠 | 每轮读全表写半表 | sumcheck 的伴随算子 |
| 5 | OP-10 MSM（Pippenger） | 每段仅 ~6% | **后置**（实测反转了原规格书的优先级） |

### 6.2 demo 数据规模建议（先小后大）

- **第一步**：单段（chunk）单算子对拍，用 s=1024 的查表族节点网格
  （requant ~2^16、rmsnorm ~2^20、softmax [4,1024,512]=2^21）——HBM 单表 ≤ 128MB，安全
- **不要**第一枪就上 V-gather 的 K'（2^27 = 4GB > HBM 单通道舒适区）
- 对拍向量来源：`ZKML_DUMP_GKR_CHALS=1` 跑 e2e（prove 侧 dump 全部就绪），
  或直接在单元测试里造随机 MLE（位序约定见 §3.3）

### 6.3 三方对拍工作流

```
CPU(Rust 现有实现) ──dump──> 测试向量(JSON/bin)
        │                          │
        │                          ├─> 3090 CUDA 参考实现（快速迭代，验证算法）
        │                          │        │ 对拍
        │                          └─> U250 XRT kernel（Vitis HLS/RTL）
        └──────────── 逐字节比对（Montgomery 形式或 canonical 形式统一即可）
```

- 3090 的价值：在写 RTL 前**先用 CUDA 把 kernel 语义写对**（BN254 Fr Montgomery
  的坑：montgomery 化/去化、约减延迟），再移植 HLS/RTL
- U250 侧：Vitis HLS 起步（C++ 模拟快）→ RTL 收尾；XRT host 用 C++/Python

### 6.4 里程碑建议

| 里程碑 | 内容 | 预估 |
| --- | --- | --- |
| M1 | U250 环境：XRT + xbutil query + host 读写 HBM 跑通 hello-AXI | 1 周 |
| M2 | OP-01 Fr 乘法器（Montgomery）+ 对拍 | 1-2 周 |
| M3 | OP-05/07 eq-加权 MLE 求值 kernel + 2^20 规模对拍 | 2 周 |
| M4 | 接入 prove_task：替换查表族 linking sumcheck 的单轮求值（bridge 原型，CPU 出挑战/收消息） | 2 周 |
| M5 | OP-13 LogUp 轮引擎 + 端到端单段对拍 + 3090/U250 吞吐对照 | 2-3 周 |

---

## 7 风险与坑清单（本会话实证教训 → FPGA 侧预防）

1. **位序**（§3.3）：kernel 索引方向与 CPU 不一致 → 对拍失败。先跑探针测试。
2. **padded/unpadded**（§3.4）：任何表生成都用 padded 网格长度；非 2 幂长度进
   MLE 构造 = panic。
3. **挑战派生留 CPU**（K 文档）：blake3 transcript 是串行关键路径（OP-03 规格），
   FPGA 只做纯函数计算段；host 侧采样的顺序一个都不能动。
4. **claim 点长度 = poly 变量数**（#3 bug）：跨算子传 claim 时段归属别混。
5. **row/col 位段归属**：mask 教训——sumcheck_point 前半 = 低变量（LSB-first），
   矩形网格的行/列段按 §3.3 推导，不要凭直觉。
6. **HBM 带宽是主约束**：2^21 点 × 8 MLE × 32B = 512MB/轮，多轮流水要算好；
   demo 先小规模（≤2^21）。
7. **α 新算子不在旧规格书里**：GatherRows/ScoreWindow/矩形 mask 是本会话新增，
   OP 清单（OP-01~14）没覆盖它们的 GKR/sumcheck 形态——若 demo 要碰 banded
   attention 段，先补规格（复用 OP-07 引擎，注意 K' poly 2^27 的规模）。

---

## 8 CPU 侧遗留（不阻塞 demo，另一会话继续）

1. **e2e 验证阶段**：chunk idx 2 (Chunk(43)) 的 17 轮 multi-size lookup GKR
   round-0 一致性（诊断数据/dump 机制全就位，见 §3.1）。
2. **全量 lib 测试重跑**（上次跑时被 e2e 调试打断）。
3. **P1 hunt v9**（claim 链 errstruct 猎捕）已运行 2 天+，输出在 cargo test harness 里。

---

## 9 附：本会话提交清单（feat/streaming-trace-prove）

```
79d0f1e9 fix(alpha): GKR 挑战推导改为独立 transcript 双侧镜像
02b25199 fix(alpha): prove_gather 重复创建 gkr_transcript 抹掉 cc/csc absorb
b107098b fix(alpha): gather input_claims 误用 lookup 点（27 维 vs 18 变量）
3d23cb16 fix(alpha): AttentionMask 矩形网格支持矩形输入 [h,q,w]
ecd30877 fix(alpha): SW gather clamp 统一为 unpadded s
02993cd3 fix(pcs): RLC sumcheck 的 max_num_vars 覆盖 eq 多项式
96bbb6b8 fix(alpha): mask row lt cycle bug + 位序约定实证 + 矩形 mask 收尾
324e4172 fix(alpha): verifier 侧补 GatherRows dispatch
0a7b1973 fix(alpha): verify_gather 表索引闭式核对改 rows_pad——修复 "1023" panic
```
