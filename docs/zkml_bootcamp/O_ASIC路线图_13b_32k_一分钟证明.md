# O · ASIC 路线图：Llama-13B / 32k / 证明 ≤ 1 分钟

> 定位：以高性能 FPGA/ASIC 架构师视角回答"研究怎么展开、从哪些维度推进"。
> 上游输入：N 文档（实测瓶颈：查表族 82%、每段成本）、FPGA 文档（算子规格 OP-01..14、
> transcript 等价生死线）、L 文档（SWA 可行性：7B/16k SWA trace 1.2-1.4TB）、
> K 文档（域固化审计）。本文不复述规格，只补**系统级预算、维度分解与里程碑**。

---

## 1 目标的物理预算（先算术，再路线）

**目标**：Llama-13B（40 层，d=5120，Hq=40）· s=32k · 证明 ≤ 60s。

### 1.1 体量前提：必须 SWA

Llama-2-13b 无原生 SWA。全注意力 s=32k 的 trace = **67.44TB Fr**（F 文档 §4）——
仅 i64 trace 就 42TB，**数据本身无法存在于任何单机**。加 SWA(w=512) 后：

| 项 | 全注意力 | SWA(w=512) |
| --- | ---: | ---: |
| score 张量 | Hq·s² = 40·32k² ×40 层 ≈ 43TB Fr | Hq·s·w ≈ **0.86TB Fr**（/50） |
| 线性项（[s,D] 系） | ≈2.6TB Fr | ≈2.6TB Fr |
| **T_pad 合计（pad 1.3×）** | 67.4TB | **≈3.5TB Fr** |

（若采用 llama3.1 式 w=4096：T_pad ≈ 9.5TB，结论同级放大 2.7×，仍可行。）

### 1.2 60 秒预算拆解（SWA w=512 口径，T_pad=3.5TB）

| 资源 | 需求 | 现代硬件基准 | 判定 |
| --- | --- | --- | --- |
| HBM 流量（折叠加载 ≈3×T_pad） | **10.5TB/60s = 175GB/s** | HBM3e 单颗 1.2TB/s | ✅ 富余 7× |
| BN254 乘法（证明工作 ≈6×T_pad） | **21T 次/60s = 350G mult/s** | ASIC MSU ≈1-2G mult/s（Cysic/Ingonyama 级） | ⚠️ 需 **300-500 个 MSU**（7nm ≈300-600mm²，单大 die 或 2-4 chiplet） |
| 推理 int8（13B×2×32k） | 0.85T OPs/60s = **14 TOPS** | 任意 NPU 核 | ✅ 可忽略（与证明共享 die） |
| CPU 角色 | transcript 锚定/调度/verify | — | ✅ 秒级 |

**判定：物理可行，但成立的前提是三个架构级决策**（缺一则预算崩塌）：
1. **SWA**（否则数据不存在）——协议与推理同步改造；
2. **零物化流式（"即算即证"）**——trace 不落任何存储，NPU 产出的 activation 经
   fieldize 直接进折叠引擎（A3 的 ASIC 终态）；
3. **推理与证明同芯**——trace 不出芯片，CPU↔ASIC 只传 claim/transcript（KB-MB 级）。

对照现状锚点：CPU（128 核）当前吞吐 ≈1.2M elem/s（N 文档 §1.1），目标吞吐
3.5TB/60s ≈ **58G elem/s——5 万×**。这不是"CPU 加速"能填的鸿沟，是架构代际差。

---

## 2 六大工作维度（展开顺序即依赖顺序）

### D1 协议维度（决定 350G mult/s 是否成立——最优先）

| 工作项 | 内容 | 验收 |
| --- | --- | --- |
| D1.1 SWA 证明协议 | L 文档评估落地：score 张量 `[Hq,s,w]` 的 sumcheck/logUp 改造 + RoPE 位置查表 | Gemma-3-1B@32k 端到端（原生 SWA，L 文档建议的首个目标） |
| D1.2 查表族常数压减 | A1（跨节点 LogUp 批处理）+ A2（einsum+requant 融合）+ 稀疏 multiplicity——82% 成本族的常数项 | 段证明时间 -50% 以上（CPU 上先验证，M2 前完成） |
| D1.3 段折叠（folding） | 48 段→超段折叠，固定开销（transcript/challenge storage/opening）占比 <1% | 折叠后 32k 全程段数 ≤8 |
| D1.4 域选择重审（ASIC 决策点） | K 文档固化 BN254 的理由是软件 grinding 余量；ASIC 有真随机源+并行挑战采样 → Goldilocks(64-bit)/BaseFold 的 4× 乘法优势重审。**必须在 M2（FPGA）前定**，改域影响全部 IP | 决策备忘录 + 双域 microbench（FPGA） |

### D2 数据流架构（零物化"即算即证"）

- **流水结构**：NPU(int8 gemm) → requant 查表（与 lookup witness 生成**同源共享表**）
  → fieldize(i64→Fr) → 折叠引擎 → GKR → claim。全链无 trace 物化（A3 的硬件形态）；
- **段间并行**：方案 A 种子链只约束 transcript 吸收顺序——**证明可乱序、按序吸收**，
  N 个 proof 引擎并行处理不同超段，FIFO 锚定；
- **背压与工作集**：任一时刻片上驻留 ≤ 单超段工作集（~100GB HBM 内），与总体量解耦
  （流式已有的"pin-dry"语义在硬件上是双缓冲乒乓）。
- 验收：M2 上 FPGA 实测"证明吞吐与 s 解耦"（s=8k 与 s=32k 的 elem/s 相差 <10%）。

### D3 算子 IP（按 N 文档实测优先级，规格沿用 FPGA 文档）

| 优先 | IP | 规格要点 | 资源占比（实测依据） |
| --- | --- | --- | --- |
| P0-A | **OP-07 sumcheck 折叠引擎**（重点 linking 形态） | fix_low 相邻对合并全流水；轮内读-算-写带宽=2×表宽；折叠树由 2 叉改 k 叉（k=SRAM 宽度）降 HBM 往返 | 查表族 82% 的主体 |
| P0-A | **OP-13 LogUp GKR** | 分数电路流水；与 OP-07 共享折叠骨架 | 同上 |
| P0-B | OP-01 Fr 蒙哥马利乘法器阵列（ASIC 版） | 300-500 MSU；Montgomery 流水或 Karatsuba；DSP-free（7nm 用 LUT+pipeline） | 地基 |
| P0-B | OP-03 Blake3 常驻 | 采样延迟 <1µs（每轮挑战） | 关键路径 |
| P1 | OP-10/11 MSM/KZG 打开 | Pippenger 阵列、点加流水；占比 6%（尾延迟） | — |
| P2 | OP-12 查表 witness 生成 | 与 NPU 的 requant 共享（一次查表两用） | 段内小头 |
| 暂缓 | OP-14 卷积 FFT | LLM 无卷积 | — |

### D4 存储与带宽体系

- **HBM 容量**：零物化后驻留 = 单超段工作集 + 权重常驻。权重 int8 13GB +
  Fr 化权重（D1.4 决定是否驻留 vs 逐层重算）≈ 30-60GB → **2-4 颗 32GB HBM3e**；
- **片上 SRAM 梯队**：折叠工作集按轮减半——L1 级 SRAM（20-40MB）承载单轮全表，
  HBM 只承载跨轮的减半序列（总 HBM 往返 = 2×T_pad）；
- **CPU↔ASIC 通道**：PCIe Gen5 x16（64GB/s）只传 claim/transcript/seed——按
  60s 预算 <0.1% 带宽；
- **多芯片**：若单 die 放不下 500 MSU → 2-4 chiplet + UCIe/Die-to-Die（≥2TB/s），
  折叠树按 chiplet 切分（claim 聚合在根 chiplet）。

### D5 软硬协同

- **transcript 逐字节等价**（FPGA 文档 §2.3 生死线的 ASIC 延伸）：Blake3 核与主机
  golden dump 逐字节对齐是 L4 验收的必要条件；S1（连续采样同值）修复必须在硬件
  冻结前完成，否则 transcript 语义固化一个怪癖；
- **证明图编译器**：当前 `HandleLifetimeRunner`（pin/dry 生命周期）→ 硬件调度器的
  指令流生成（op 拓扑 + 内存生命周期 + 折叠树形状）；
- **CPU 侧定位收缩**：A4/A3 后 CPU 只剩 推理备援/调度/verify——13b/512 已 4h44m，
  无需再优化 CPU 证明本体（投资全部转向 IP）。

### D6 验证与交付方法学

- L1 算子级 → L2 实例级 → L3 chunk 级 → L4 端到端（FPGA 文档 §2.4），
  ASIC 阶段加 L5：网表级 vs golden 的形式等价（SymbiYosys/CTL）；
- 每级有**功耗/面积预算表**（MSU 阵列 60% 面积、HBM PHY 20%、SRAM 15%、控制 5%）；
- 失败预算：P1 类"诚实证明被拒"的可靠性问题在硬件上不可调试——CPU 侧必须先归零。

---

## 3 硬件架构基线（M4 目标形态）

```
┌──────────────────── ASIC（7nm，~400-600mm²，或 2×chiplet）────────────────────┐
│  NPU: int8 gemm 阵列（14 TOPS）   requant 查表（与 OP-12 共享）                 │
│  ↓ i64 activation（不出片）                                                     │
│  fieldize → [SRAM 32MB] → 折叠树引擎 ×4（OP-07/13，各带 64 MSU）                │
│                             ↓ claims                                            │
│  MSU 阵列 ×300-500（OP-01）← HBM3e ×4（32GB，175GB/s 均值）                    │
│  Pippenger MSM（OP-10/11）   Blake3 常驻（OP-03，<1µs 采样）                    │
│  PCIe Gen5 x16 ↔ CPU（claim/transcript/seed，<0.1% 带宽）                       │
└──────────────────────────────────────────────────────────────────────────────┘
```

预算表（60s/32k）：折叠 25W、MSU 40W、HBM 15W、NPU 5W → **TDP ~100W** 单芯片。

---

## 4 里程碑（每级量化验收，逐级去风险）

| 级 | 内容 | 验收 | 预计 |
| --- | --- | --- | --- |
| **M0**（已达） | CPU 全链基线：13b/512 PASS（A4 后 4h44m）、7b/1024 PASS、吞吐/算子画像齐全 | N 文档数据 | ✅ |
| **M1** | CPU 侧协议改造：D1.1 SWA（Gemma-1B@32k 端到端）+ **A2 einsum+requant 融合**（A1 经核算降级 P2——GKR 批量化不省体积比例项）+ S1 修复 | 13b/512 证明 ≤2h（A2 后；≤30min 由 M2 FPGA 兑现）；Gemma-1B/32k PASS；失败率归零 | 4-6 周 |
| **M2** | **单 FPGA**（UL3524 级：HBM 400GB/s + 8-16 MSU）：OP-01/03/07/13 上板，D1.4 域决策 | Gemma-270m/8192 证明 ≤10min；13b/512 ≤60min；L1-L4 全过 | 3-4 月 |
| **M3** | 多 FPGA（4×）+ NPU 原型：段间并行 + 即算即证数据流 | 13b/4k 证明 ≤10min；吞吐与 s 解耦实证 | +3 月 |
| **M4** | **ASIC**（300-500 MSU + NPU + HBM3e ×4） | **13b/32k 证明 ≤60s**；L5 网表等价 | 流片 9-12 月 |

**关键路径**：D1.1 SWA → M1 → D1.4 域决策 → M2 → 数据流验证 → M4 流片。
**CPU 侧在 M1 后冻结投入**（M2 起 CPU 只做 golden model 与调度）。

---

## 5 风险与决策点

| # | 风险 | 缓解 |
| --- | --- | --- |
| R1 | P1 偶发失败未归零就上硬件 | M1 前置门禁：失败率归零（猎捕继续） |
| R2 | 域选择（D1.4）翻烧饼 | M2 前双域 microbench 一次定案；IP 抽象域参数 |
| R3 | Llama-2 无原生 SWA | 两条腿：Gemma-3 系先行（原生 SWA）；或 Llama2 加 SWA 的证明协议层等价改造（推理数值会变——需确认量化方案接受） |
| R4 | 500 MSU 单 die 面积/良率 | chiplet 备选路线（UCIe），折叠树已可按 die 切 |
| R5 | 1 分钟目标被"非证明"环节吃掉（verify/IO/transcript） | 全链每环节秒级预算表，M3 起逐项实测 |
| R6 | 模型升级换代（llama→其他） | 编译器吃 ONNX/图 IR，IP 与模型解耦 |

---

## 6 与现有文档的衔接

- 算子规格/寄存器级契约 → **FPGA 文档**（OP-01..14，M2 直接消费）；
- 实测优先级与段成本 → **N 文档**（§3.2 已反转 MSM/sumcheck 顺序）；
- SWA 体积推导 → **L 文档**（Gemma-1B@32k 首目标建议沿用）；
- 域固化论证 → **K 文档**（D1.4 的重审对象）；
- CPU 基线与失败台账 → **M 文档**（M0 锚点）。
