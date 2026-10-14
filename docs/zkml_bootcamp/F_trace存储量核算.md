# F · ZK prove 阶段输入（Trace）存储量核算

> **定位**：只回答一个问题——**ZK 证明器要消费的那份 Trace 到底多大**。只算存储量，不做推理阶段的时序/延迟评估。
> **读者**：FPGA 架构师、内存规划者——用于确定存 Trace 需要多大内存、如何传给加速器。
> **口径**：⚠️ 前提是你说的——**Trace 在 FPGA 上按 `i64`（8 B/元素）存储，需要参与 ZK 运算时才 `i64 → Fr`（32 B/元素）**。因此本文件全程按 i64 计字节，Fr 只在"转换后占用"里提一句。

---

## 0. 结论先行

**ZK prove 的输入是一整份 trace，大小 ≈ 一次带完整序列「前向」的所有中间张量之和 × 8 B。**

对 GPT-2（124M, 隐藏 768, 12 层）：

| 序列 s | Trace 大小（i64, 8B） | 占用占比 |
|---|---:|---|
| 128 | **≈ 0.2 GB** | 线性主导 |
| 512 | **≈ 1.3 GB** | score 二次项开始抬头 |
| 1024 | **≈ 3.8 GB** | score 二次项 ≥ 线性部分 |

**关键判断**：trace 不是逐 token 累加出来的（见 §1），所以它只有一个 `s` 的量级，且按 i64 存只有权重（Fr）的约 1/4。**trace 不是内存瓶颈，权重才是**（GPT-2 权重 ≈ 3.97 GB Fr）。但当 `s` 上到 1024+，注意力 score 的二次项（`2304·s²`）会把 trace 追到 GB 级、与权重同量级。指标公式见 §4：`Trace ≈ 1.29M·s + 2,304·s² B`（i64）。

---

## 1. Trace 是怎么来的（决定体积的前提）

**证明用的 trace 不是逐 token 拼接，而是最后一次带完整序列的一次前向。**

- 逐 token 循环（[model/llm.rs:495-511](../../zkml/src/model/llm.rs#L495-L511)）只用于**生成 token + 累积 kv-cache**，不生产最终 trace。
- 真正产出 trace 的是最后这一跑（[model/llm.rs:556-572](../../zkml/src/model/llm.rs#L556-L572)）：
  ```rust
  let tensor = Tensor::new(Shape::new(vec![full_sentence.len()]), full_sentence.clone())?; // [s]
  ...
  let trace = self.model.run_with_split_nodes_info(vec![tensor], store, ...)?;
  ```
  这次 `input_shape = [s]`，所以**每个图节点的输出张量第一维 = 序列长 s**。

→ 因此 trace 大小 = **一个 s 长的前向中，被记录的每个节点输入/输出/辅助量之和**，不是 ×token 叠加。

---

## 2. Trace 里到底存什么

`Trace<D>`（[trace.rs:29](../../zkml/src/model/trace.rs#L29)）`steps: HashMap<NodeId, Step>`，每个 `Step`（[trace.rs:388](trace.rs#L388)）：

- `node_inputs`：该节点输入端口收到的张量
- `node_outputs`：该节点算出的输出张量（**第一维 = s**）
- `node_outputs.proving_data`：层内辅助量（LayerNorm 的 mean/std、softmax shift、argmax 值等）

类型 **`Element = i64`（8 B）**（[lib.rs:103](../../zkml/src/lib.rs#L103)）。

---

## 3. 每层被记录的中间张量（GPT-2 一列）

以一个 transformer block 为例（[decoder/mod.rs:35-80](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L35) + [attention_layer.rs:140-198](../../zkml/src/parser/llm/transformer/attention_layer.rs#L140)）：

| 节点 | 输出 shape | 元素数(每层) | i64 字节 |
|---|---:|---:|---:|
| ln_1 | `[s, 768]` | 768·s | 6.1K·s |
| QKV einsum（Q/K/V） | 3×`[12, s, 64]` | 2304·s | 18.4K·s |
| query-key score | `[12, s, s]` | 12·s² | 96·s² |
| softmax / mask | `[12, s, s]` | 12·s² | 96·s² |
| attention-value | `[12, s, 64]` | 768·s | 6.1K·s |
| output einsum | `[s, 768]` | 768·s | 6.1K·s |
| 残差 add | `[s, 768]` | 768·s | 6.1K·s |
| ln_2 | `[s, 768]` | 768·s | 6.1K·s |
| c_fc einsum | `[s, 3072]` | 3072·s | 24.6K·s |
| GELU | `[s, 3072]` | 3072·s | 24.6K·s |
| c_proj einsum | `[s, 768]` | 768·s | 6.1K·s |
| 残差 add | `[s, 768]` | 768·s | 6.1K·s |

> 首尾另有：embeddings `[s,768]`、positional `[s,768]`、ln_f `[s,768]`、final_proj→argmax（`[s, 50257→取 logits 末 token]`，见 [model/llm.rs:537](../../zkml/src/model/llm.rs#L537) 只证明末 token，argmax 输出不按 s 放大）。

---

## 4. 核算公式

**每层**（i64，8B）：
- **线性/向量部分**（∝ s）：`V = Σ(每节点元素) × 8 B = c_v · s`
  - GPT-2 `c_v = (5×768 + 2×3072 + 2304·QKV + 768·value) × 8 = 13,056 × 8 = 104,448 B ≈ 102 K B`（每层每 token 当量；5 个 768 向量 + 2 个 3072 + QKV + value，见 §3）
- **注意力 score 部分**（∝ s²）：`S = (query-key + softmax) × 8 B = 24 · s² · 8 = 192 · s² B`
  - 口径：mask 就地并入 softmax（见 attention_layer.rs），不另物化独立 trace 节点；故记 **两份** score。§3 表与之一致。

**整份 trace**（i64）：
```
Trace ≈ (12 层 × V) + (12 层 × S) + 首尾(≈6·768·s·8)
     = 12·(104448·s + 192·s²) + 36,864·s
     ≈ 1.29 M·s + 2,304·s²  (B, i64)
```

> `c_v` 因模型而异（隐藏维、FFN 倍率、头数/头维）。关键是**线性项 ∝ s、score 项 ∝ s²**：s 越大，二次项越主导。

### 4.1 通用公式（任意模型，可自己代入算）

用 GPT-2 校准出两个"按模型维度缩放"的常数（i64）：
- 每层线性当量 `c_v ≈ 17 × D`（B/token/层）——GPT-2: `17×768 = 13,056` ✓；它= "每层中间张量相当于 ~17 个隐藏维向量"。
- 每层 score 当量 `S = 16 × h × s²`（B/层）——GPT-2: `16×12×s² = 192·s²` ✓（score 记 query-key+softmax 两份）。
  > 这里的 `h` 指 **query 头数 Hq**（源码：query-key einsum 输出 `[Hq,s,s]`），**不是总头数也非 kv 头数**。GQA/MQA 的 kv 头数更少只砍 K/V 线性项、不砍 score 二次项。可比 trace 内存由 Hq 主导而非 kv_heads 主导——§5.1 真实模型表已验证（Gemma Hq=4 → 存储远小于 Llama Hq=32）。

**整份 trace（i64，通用）**：
```
Trace(s) ≈ [16 · L · h · s²] + [17 · L · D · s] + 首尾(≈6·D·8·s)      B
         (score 二次项)      (线性/向量项)          (emb+pos+ln_f)
```
- 只依赖 `L`(层)、`D`(隐藏维)、`h`(头数)、`s` 四个量；任意 1B/7B 模型代入即得。__得分占比随 s 增长：s 大到 score 项主导时（通常 s ≥ 数百），trace 由 h、L、s² 决定，D 退居次席__。

---

## 5. 常见模型 × 通用上下文（i64，单位 GB）

> 全表由 `tools/trace_size.py` 按**真实 config 维度逐节点核算**（GPT-2 锚点自检命中 §0）。数值口径统一：score 二次项只随 **query 头数 Hq** 走，FFN 按 SwiGLU（gate+up+激活）计，GPT-2 为标准 MLP。

### 5.1 所有模型的真实维度（来自权威 config）

> 维度来源：GPT-2 / Llama-2-* / Gemma-3-1B 来自**仓库内 config.json**；**Llama-3.2-1B、Qwen2-1.5B 直接取自 ModelScope 官方 config**（`LLM-Research/Llama-3.2-1B`、`Qwen/Qwen2-1.5B`，已存档于 [refs/](refs/)）。可复现脚本：`tools/trace_size.py`（GPT-2 锚点自检通过，s=512 → 1.29GB ✓ 与 §0 一致）。**DeepSeek 无官方 dense 1B 原生模型，故不入表**（见 5.3 脚注）。

| 模型 | 参量(approx) | D | L | 头数(q/kv) | head_dim | inter | FFN 类型 | 权重(Fr) |
|---|---|---:|---:|---:|---:|---:|---|---:|
| **GPT-2** (124M) | 0.15B | 768 | 12 | 12 / 12 (MHA) | 64 | 3072 | 标准 MLP (tied) | 0.6 GB |
| **Llama-2-7B** | 6.7B | 4096 | 32 | 32 / 32 (MHA) | 128 | 11008 | SwiGLU | 27.0 GB |
| **Llama-2-13B** | 13.0B | 5120 | 40 | 40 / 40 (MHA) | 128 | 13824 | SwiGLU | 52.1 GB |
| **Llama-3.2-1B** | 1.2B | 2048 | 16 | 32 / **8**(GQA) | 64 | 8192 | SwiGLU (tied) | 4.9 GB |
| **Qwen2-1.5B** | 1.8B | 1536 | 28 | 12 / **2**(GQA) | 128 | 8960 | SwiGLU | 7.1 GB |
| **Gemma-3-1B** | 1.3B | 1152 | 26 | **4** / 1(MQA) | 256 | 6912 | SwiGLU | 5.2 GB |

> 参数/权重按 script `params()`（embedding + 每层 qkv+out+3·D·inter，Fr 32B）。**Gemma 的 Hq·hd = 4×256 = 1024 ≠ D = 1152**（query 投影降维，见源码 `gemma3/decoder/attention.rs`），其余模型 Hq·hd = D。

### 5.2 每个模型的整份 trace 内存（i64，GB，按序列长度 s）

| 模型 | s=128 | s=512 | s=1024 | s=2048 | **s=4096** | s=8192 |
|---|---:|---:|---:|---:|---:|---:|
| GPT-2 (0.15B) | 0.2 | 1.3 | **3.8** | 12.4 | **44.2** | 165.6 |
| Llama-2-7B | 2.7 | 14.0 | 36.7 | 107.7 | **352.9** | 1255.5 |
| Llama-2-13B | 4.2 | 22.0 | 57.3 | 168.4 | **551.5** | 1961.9 |
| Llama-3.2-1B | 0.8 | 4.9 | 14.1 | 45.5 | **159.7** | 594.2 |
| Qwen2-1.5B | 1.2 | 6.0 | 14.8 | 40.8 | **126.8** | 433.9 |
| Gemma-3-1B | 0.8 | 3.7 | 8.2 | 19.9 | **53.8** | 163.4 |

> **读法**：同 s=4096，横跨 12 倍——44GB（GPT-2）到 551GB（Llama-2-13B），由 **Hq（query 头数）× L（层数）** 的 score 二次项和线性项合力决定。**三个 1B 档各不同（53.8 ≤ ~160GB），印证"1B"非单一内存画像**；分子放大到 13B 后 s=4096 破半 TB，s=8192 逼近 2 TB。s 越大，score 二次项越主导（下表当量）。

### 5.3 score 二次项在各 s 的主导程度（与前表同口径）

| 模型（Hq·L） | s=128 | s=512 | s=1024 | s=4096 |
|---|---:|---:|---:|---:|
| GPT-2（12·12） | 18% | 47% | 64% | 88% |
| Llama-3.2-1B（32·16） | 16% | 44% | 61% | 86% |
| Gemma-3-1B（4·26） | 3% | 12% | 21% | 52% |

> score 项 ∝ **Hq·s²**（源码：query-key einsum 输出 `[Hq,s,s]`，第 2 份为 mask/softmax）。**决定性变量 = query 头数 Hq**，而非总头数/kv 头数——GQA/MQA 的少 kv 头只砍 K/V 线性项、不砍 score 二次项。Gemma Hq=4 最轻，s=4096 只要 ~54GB。

**对 FPGA 内存的判定**（三个 1B 档）：
- **@ s=4096 全部超单板**（U280 ~4.6GB HBM，[C](C_FPGA实现规格.md)附录B；转 Fr（×4）更是 214–638GB），单板不成立。
- **score 二次项一骑绝尘**（52–88%@s=4096），线性项 + 权重（5–7 GB Fr）可忽略。**1B 档 trace 天花板 = 注意力 score，不在权重。**
- **Gemma-3-1B 最轻（Hq=4）**；其 local-attention 窗口 512（config `sliding_window`）令 score 稀疏带，若能按窗裁剪可再压到个位数 GB，是三个里唯一"单板可谈"的候选。
- **重要**：Gemma/Llama/Qwen score 张量物理上**仍是 `[Hq,s,s]` 全物化**（mask 只就地置 -inf、不改 shape，[attention_layer.rs:190](../zkml/src/parser/llm/transformer/attention_layer.rs#L190)），sliding-window/下三角**不减少 trace 存储**，除非 FPGA 侧额外做稀疏/分块裁剪（见 [D](D_prove算法全流程详解.md) 附录 C 风险）。

> **DeepSeek 脚注**：DeepSeek 系无官方 dense 1B（最小 dense 为 V2/V3-Lite 16B·MoE）；其 1.5B 档用 **R1-Distill-Qwen-1.5B**，架构即上表 Qwen2-1.5B 行，trace 内存同 Qwen2-1.5B。

## 6. 对 FPGA 内置的含义（一句话）

- **存 trace 按 i64 → 8 B**，转换到 Fr（32 B）只有到 sumcheck/MSM 真正消费某节点时才发生；不需整份预转 Fr（那会 ×4）。
- **带宽**：trace 是"只读一次"的规则流，可按 chunk（2²⁰ 元素 = 8 MB i64 / 32 MB Fr）流式灌给加速器；权重和 trace 一起构成在 HBM 上的两张表。
- **瓶颈判定**：对 GPT-2 级，trace(i64)≪权重(Fr)，内存看权重；`s` 长到 1024+，score 二次项把 trace 推到与权重同量级，须把 attention 的 score 作为二次项单独预算（与 C 报告风险一致）。

---

## 7. 一次 prove 的 MSM 次数与 sumcheck 实例数（由 trace 直接导出）

> **定位**：上文只算"trace 占多大内存"。本节顺着它把**这些 trace 在 prove 里会驱动多少次 MSM / sumcheck 调用**数出来——因为 FPGA 加速的抓手就是这几类算子的调用次数与数据量。**口径全部复用 §5.2 的 trace GB 表，是它的直接消费侧**。算法链路见 [D](D_prove算法全流程详解.md) 附录 A/[H](H_prove全算子清单与数据流.md) 段8。
>
> **两条根本规律先记住**：
> - **MSM 次数 ∝ 域化元素总量 ΣQ**（随 trace 的 `s+s²` 涨）；
> - **sumcheck 实例数 ∝ 图节点拓扑（einsum/查表层数量），不随 s 涨**——但**每个实例的轮规模 ∝ 该节点张量 2^n**，所以 sumcheck 的"总代价"仍随 s 涨，只是"实例数"不涨。

### 7.1 MSM 次数（witness 批量承诺块数）

**批量承诺**（[context.rs:58](../../zkml/src/poly_commit/context.rs#L58)）：每个多项式按 `min(ceil(2^num_vars / 2^20), 128)` 切块，**每块 = 一次 size-2^20 的 Pippenger MSM**。元素数与 trace i64 元素数等价（域化不改元素数），故：

```
MSM批量块数 ≈ ΣQ / 2^20 ，  ΣQ = trace 域化元素总数 = Trace_字节(i64) / 8（F §4）
```

> 打开（KZG）另加：每 chunk 每打开 1 条 rlc_poly，**额外 ≈ 该条 MLE 变量数 m 次的折叠/tie MSM**（dp-crypto `arkyper/mod.rs:362`，位于 `~/.cargo/git/checkouts/dp-crypto-*`，不在仓内）。默认整模型 1 chunk ⇒ 1 次合并打开。

**各模型批量块数**（由 §5.2 的 trace GB 用 `GB×10⁹/8/2^20` 导出，单位：次）：

| 模型 | s=128 | s=512 | s=1024 | s=2048 | s=4096 | s=8192 |
|---|---:|---:|---:|---:|---:|---:|
| GPT-2 (0.15B, D768·L12·12头) | 24 | 155 | 453 | 1478 | 5269 | 19741 |
| Llama-2-7B (D4096·L32·32头) | 322 | 1669 | 4375 | 12839 | 42069 | 149667 |
| Llama-2-13B (D5120·L40·40头) | 501 | 2623 | 6831 | 20075 | 65744 | 233877 |
| Llama-3.2-1B (D2048·L16·32q/8kv) | 95 | 584 | 1681 | 5424 | 19038 | 70834 |
| Qwen2-1.5B (D1536·L28·12q/2kv) | 143 | 715 | 1764 | 4864 | 15116 | 51725 |
| Gemma-3-1B (D1152·L26·4q/1kv) | 95 | 441 | 978 | 2372 | 6413 | 19479 |

> **读数**：MSM 次数 = trace 规模/2^20，**几乎与参数量无直接关系**，只与 ΣQ 有关；s 翻倍时 score 二次项把块数拉高 ~4×（GPT-2 s=512→4096：155→453→5269……正是 `s²` 主导，对应 §5.3 score 占比）。每个块 = 8 MB i64 / 32 MB Fr，是 FPGA 双缓冲乒乓的建议灌入粒度（[D](D_prove算法全流程详解.md) 附录 C）。

### 7.2 sumcheck 实例数（节点级 + 每 chunk 1 次合并）

sumcheck 实例来自**可证明节点的类型**（源码逐节点核定，见 [H](H_prove全算子清单与数据流.md) 段4/4c）——**不是每一层调一次，而是每个节点调自己的那几次**：

```
实例数 ≈ Σ_每个可证明节点(该节点自身的 sumcheck 数次)  +  C（每 chunk 1 次合并，[prover.rs:140](../../zkml/src/poly_commit/prover.rs#L140)）
```

| 节点类型 | 每节点的 sumcheck 实例数 | 源码 |
|---|---|---|
| einsum | **1 主** +（LHS 多 claim 时）+1 input_agg | einsum/prove.rs:184/288 |
| 查表层（GELU/softmax/LN/RMS/SiLU） | **LogUp GKR = `total_layers` 轮实例** + **1 链接** | logup_gkr/prover.rs:183 + generic_prove.rs:222 |
| embedding | 1 | embeddings.rs:443 |
| split | 同点→0；异点→每输入 1 | split.rs:388/432 |
| conv（F FT批量） | 多个 | conv/mod.rs:762/833 |

> **为什么"查表层每个 = d_L+1 个实例"——d_L 是哪来的（必须先懂这一步，否则 7.2.2 的表没法读）：**
>
> 一个查表层节点（GELU/softmax/LN/RMS/SiLU）不是"一个 sumcheck 就证明完查表关系"，而是用 **LogUp（log-up folding）** 逐变量折叠把"查表 + 多重度计数"证出来。折叠 1 个变量 = 1 次 sumcheck，变量轮数由表多大决定。源码 [logup_gkr/prover.rs:183](zkml/src/lookup/logup_gkr/prover.rs#L183)：
>
> ```rust
> let total_layers = 该批查表 MLE 的最大 num_vars();   // log₂(查表层张量元素数)
> for current_layer_vars in 1..=total_layers {          // 每轮 1 次 sumcheck
>     ...                                                // 折叠一个变量
> }
> // 之后还 +1 个 linking sumcheck 把各变量收口 → 合计 d_L+1 个
> ```
>
> - **`d_L` = `total_layers` = log₂(查表层张量元素数)** = 这个查表节点自己 LogUp 折叠的轮数。它只由**查的表多大 / 量化位宽**决定，**不随序列长 s 变**。
> - 每个查表层贡献 **`d_L` 轮 + 1 链接 = `d_L+1` 个 sumcheck 实例**。比如 GELU 表按输入区间+量化桶展开，d_L≈8–20；LN/RMS 按均值方差张量 d_L≈8–13 —— **对 5 个模型大致同档**，因此下面 7.2.2 的 d_L 取 8/13/20 三档。
> - 一句话：**d_L = 那个查表层节点内部自己要多跑几轮 sumcheck（≈ log₂(查表层大小)），不随 s 变，只随查的表多大变。**

#### 7.2.1 每 block 的可证节点结构（各模型都一样：注意力 4 einsum + 1 softmax；FFN 2 einsum + 1 激活）

我从 `attention_layer.rs:167-208`、`feed_forward.rs:53-116`、各模型 `decoder/mod.rs` 逐层核出：**注意力一律 4 个 einsum + 1 个 softmax**（QKV、query-key、[mask]、softmax、attention-value、output），**FFN 一律 2 个 einsum + 1 个激活查表层**（SwiGLU 的 gate×up 融合成 1 个 + down；标准 MLP 的 c_fc + c_proj）。GQA/MQA 只改**张量维度**、不改**节点数**。**唯一拉大 per-block 查表层数的，是"归一化再加几次"和"叠加几个 LN"**：

| 模型 | 每 block einsum | 每 block 查表层 | block 数 L | 首尾(emb+最终norm+最终proj) |
|---|---:|---:|---:|---|
| GPT-2 (D768·L12) | 6（QKV·score·attn-v·out·c_fc·c_proj） | **4**（ln_1·softmax·ln_2·GELU） | 12 | token-emb + **position-emb** + ln_f + final_proj |
| Llama-2-7B (L32) | 6（QKV·score·attn-v·out·ffn_glu·ffn_down） | **4**（RMSNorm×2·softmax·SiLU） | 32 | token-emb + final RMSNorm + lm_head |
| Llama-3.2-1B (L16) | 6（同上） | **4**（同上） | 16 | 同上 |
| Qwen2-1.5B (L28) | 6（同上） | **4**（同上） | 28 | 同上 |
| Gemma-3-1B (L26) | 6（同上） | **8**（RMSNorm×4·q_norm·k_norm·softmax·GeGLU） | 26 | token-emb + final RMSNorm + lm_head |

> （Llama-2-7B 缓存是 MHA(32kv)、Llama-3.2-1B/Qwen 是 GQA——**但节点数一致**；Gemma 是 MQA，同样节点数。RoPE/GQA-MQA/mask/残差 Add 都**不产生**独立的可证明 sumcheck，只是对齐张量维度/就地掩码。）

#### 7.2.2 每模型的 sumcheck 实例总数（einsum≈1，查表层=d_L+1，默认 1 chunk）

```
总和 ≈ L·[每 block einsum数 + 每 block 查表层数·(d_L+1)]  +  首尾  +  1 合并
       GPT-2 首尾+合并 = (token1+pos1+ln_f(d_L+1)+proj1)+1合并 = d_L+6
       Llama/Qwen/Gemma 首尾+合并 = (token1+finalRMSNorm(d_L+1)+lm_head1)+1合并 = d_L+5
```

| 模型 | L | 每 block 小计(块内) | d_L=8 | d_L=13 | d_L=20 |
|---|---:|---:|---:|---:|---:|
| GPT-2 | 12 | 6×12 + 4×12·(d_L+1) = 48d_L+120 | **≈518** | **≈763** | **≈1106** |
| Llama-2-7B | 32 | 6×32 + 4×32·(d_L+1) = 128d_L+320 | **≈1357** | **≈2002** | **≈2905** |
| Llama-3.2-1B | 16 | 6×16 + 4×16·(d_L+1) = 64d_L+160 | **≈685** | **≈1010** | **≈1465** |
| Qwen2-1.5B | 28 | 6×28 + 4×28·(d_L+1) = 112d_L+280 | **≈1189** | **≈1754** | **≈2545** |
| Gemma-3-1B | 26 | 6×26 + 8×26·(d_L+1) = 208d_L+364 | **≈2041** | **≈3086** | **≈4549** |

> **读法（这就是"不同模型差在哪"）**：
> - **size（参数量/B）不改变实例数** —— 实例数只按**层数 L** 线性放大：GPT-2(L12,0.15B)≈500，Llama-3.2-1B(L16,1B)≈700，Qwen(L28,1.5B)≈1200，Llama-2-7B(L32,6.7B)≈1400 —— **同样的 L 就给同样多次**，与 D/头数无关。
> - **同架构内、同 L 的两个模型实例数完全一样**（只差张量维度）；**唯一结构性差异**是 Gemma-3 每 block 查表层比别家多 4 个（q_norm·k_norm + 前置 4 个 RMSNorm）→ 同 L≈26 下实例数约是"瘦架构"的 **1.5–1.9×**。
> - 若 einsum 多 claim 触发 input_agg（+1/个），上面每行再 +L 左右（einsum 从 1→2 档）。

#### 7.2.3 为什么 README 常说"不随序列长变"

**这些实例数不随 s 变化**（只依赖节点数和 d_L）。s 变大影响的是**每个实例内部的轮数/规模**（einsum 的 `n=log₂(收缩维)`、查表层张量 2^k）——所以 sumcheck 的"**实例数**"是 GPU/FPGA 的**并行争用数**（数百~数千，可节点间并行），"**总代价 = Σ 实例 × O(2^n)**"仍随 s（score 实例的规模 s²）涨。两本账分开规划（实例数管怎么铺线程，规模管每个线程吃多少）。

### 7.3 "次数跟什么有关"一句话表

| | MSM 次数 | sumcheck 实例数 | sumcheck 总代价 |
|---|---|---|---|
| 依赖 | **ΣQ**（trace 元素量 ∝ s+s²） | **节点数 + 节点类型 + LogUp d_L**（不随 s） | **Σ 实例×实例张量 2^n**（随 s/s²） |
| 默认 1 chunk | ≈ ΣQ/2^20 + 1 打开 | Σ_节点实例 + 1 合并 | — |
| 例 GPT-2 s=1024 | ≈ 453 批 + 1 打开 | ≈ 500–1200 实例（d_L 主导） | 由 score 规模主导 |

> 对 FPGA 的合意：**「段①批量承诺」要按 §7.1 的块数上一次批量 MSM 循环，「段③sumcheck」要按 §7.2 的实例数在节点间铺并行、节点内流水拉满**；s 延长更多是让**同一个块流水线多吃数据**，而不是多拉起算子（与 [B](B_工作负载与瓶颈判定.md) §5"瓶颈随 n 转移"一致）。

---

> 本文只算"prove 输入 trace 多大"。权重存储、MSM/sumcheck 的域运算负载见 A/C。[F] 假设 i64 存、用时转 Fr；若要"FPGA 上先转 Fr 再存"则把上表 ×4。
