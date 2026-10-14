# GPT-2 代码流程导读（数据流视角）

> 阅读前提：你已经跑通 `cargo run --release --bin bench-llm -- --model gpt2 ...`。
> 这篇文章从「数据/张量从磁盘一路流到 ZK 证明」的视角，逐段带你读代码，所有引用都标注了 `文件:行号`，请对照源码阅读。
>
> 命令入口：[`zkml/src/bin/bench/llm.rs`](../../zkml/src/bin/bench/llm.rs)

---

## 0. 流程图总览

```
safetensors 权重文件
   │  (FileTensorLoader 按名取张量)
   ▼
GPT2::parse  →  LLMIR（LLM 中间表示：embeddings/blocks/ln_f/final_proj）
   │  (LLMIR::into_model → 构图)
   ▼
Model<f32>（有向图，节点是 Layer，边是张量依赖）
   │  (Driver 包一层)
   ▼
Driver : 量化 + 推理生成 Trace（run_elements）
   │        │
   │  ┌─────┴── generate_contexts (ProverCtx / VerifierCtx) ◄── 一次性，可 --save-params
   ▼  ▼
Prover::chunked_prove_local (prove) ──► Proof
   │
   ▼
Verifier::verify ──► 接受/拒绝
```

两条并行的主线：
1. **解析 & 构图**（离线，跑一次）：把磁盘权重读进来，组成一张「每步一个 ZK 层」的图。
2. **证明 & 验证**（在线，每次跑）：把推理时产生的 `Trace`（每一步的输入/输出值）交给证明器，逐层产出多项式承诺与 sumcheck 证明。

下面按数据流顺序走。

---

## 1. 命令行入口：`bench-llm`

文件：[`llm.rs`](../../zkml/src/bin/bench/llm.rs)

- `main()` 在 [llm.rs:153](../../zkml/src/bin/bench/llm.rs#L153) 开始。先配置 tracing 日志与 CSV 输出（[llm.rs:154-159](../../zkml/src/bin/bench/llm.rs#L154-L159)）。
- 命令行参数解析在 [llm.rs:59-142](../../zkml/src/bin/bench/llm.rs#L59-L142)（`LLMArgs`）：`--model gpt2`、`--hf`/`--gguf` 二者取一（`weights` group，[llm.rs:61](../../zkml/src/bin/bench/llm.rs#L61)）、`--sequence`/`--max_context` 取一（`length` group，[llm.rs:62](../../zkml/src/bin/bench/llm.rs#L62)）、`--save-params`/`--load-params`（[llm.rs:125-131](../../zkml/src/bin/bench/llm.rs#L125-L131)）。
- 线程池：`rayon` 全局线程数 = 逻辑核心数（[llm.rs:164-177](../../zkml/src/bin/bench/llm.rs#L164-L177)）。
- `--sequence 128` 会被拆成 `(126, 128)`：先喂 126 个 token 生成 2 个 token（见 [llm.rs:179-191](../../zkml/src/bin/bench/llm.rs#L179-L191) 的注释）。后面 [llm.rs:265](../../zkml/src/bin/bench/llm.rs#L265) 起按这个 sequence 逐个执行。

关键的数据流入口在 [llm.rs:203](../../zkml/src/bin/bench/llm.rs#L203)：`parse_model` 返回 `(Driver, HFTokenizer)`。

---

## 2. 模型加载：从 safetensors 到 LLMIR

### 2.1 先看数据从哪个文件来 —— `RawSafeTensors`

`parse_model` 在 [llm.rs:380](../../zkml/src/bin/bench/llm.rs#L380)。因为传的是 `--hf`，走 [llm.rs:394-418](../../zkml/src/bin/bench/llm.rs#L394-L418) 的 safe 分支：
- `RawSafeTensors::from_hugging_face_cached(hf)`（[llm.rs:395](../../zkml/src/bin/bench/llm.rs#L395)）→ 得到权重容器。
- 然后按模型名 dispatch 到 `GPT2` 并调 `Driver::load_from_model(model_type, &safe, ...)`（[llm.rs:398-402](../../zkml/src/bin/bench/llm.rs#L398-L402)）。
- tokenizer 通过 `model_type.load_tokenizer(&safe)` 加载（[llm.rs:401](../../zkml/src/bin/bench/llm.rs#L401)）。

对于 gpt2，实测你本地缓存是 `openai-community/gpt2`（单文件 `model.safetensors`）。让我把这条 safe-加载链路的具体函数也标出来：

文件：[`src/parser/safe.rs`](../../zkml/src/parser/safe.rs)
- `RawSafeTensors::from_hugging_face_cached(repo_id)`：先查 `model_cache/<repo_id>/`，命中则本地读，否则尝试下载。**注意缓存目录是相对 CWD 的**（`model_cache`），从仓库根跑和从 `zkml/` 跑会读到不同的缓存路径。
- `config.json` 解析成 `ConfigJSON`（`read_config_json`）。
- 之后所有取权重都走 `FileTensorLoader::get_tensor("tensor.name")`。

### 2.2 `GPT2::parse` —— 读权重并拼成 LLMIR

文件：[`src/parser/llm/models/gpt2/mod.rs`](../../zkml/src/parser/llm/models/gpt2/mod.rs)

`impl ModelLoader<RawSafeTensors> for GPT2::parse` 在 [gpt2/mod.rs:160](../../zkml/src/parser/llm/models/gpt2/mod.rs#L160)：

1. 读 config.json 的字段（[gpt2/mod.rs:172-183](../../zkml/src/parser/llm/models/gpt2/mod.rs#L172-L183)）：`n_embd`(768)、`n_head`(12)、`n_ctx`、`n_layer`(12)、`layer_norm_epsilon`、`vocab_size`、`eos_token_id`。
2. 建 `FileTensorLoader`（[gpt2/mod.rs:187](../../zkml/src/parser/llm/models/gpt2/mod.rs#L187)），并用第一个 FFN 的 bias shape 探测 `intermediate_size`（[gpt2/mod.rs:189-192](../../zkml/src/parser/llm/models/gpt2/mod.rs#L189-L192)）。这是 gpt2 config.json 没给 `intermediate_size` 的 hack。
3. 组装 `LLMConfig`（[gpt2/mod.rs:194-210](../../zkml/src/parser/llm/models/gpt2/mod.rs#L194-L210)），`head_size = hidden_size / num_heads`。
4. 用 `gpt2_structure`（[gpt2/mod.rs:281](../../zkml/src/parser/llm/models/gpt2/mod.rs#L281)）决定架构骨架：`NormType::LayerNorm` + `PositionalConfig::FixedPositional` + `AttentionHeadType::MHA`。→ 这一行决定了后面用 **MHA** 的 einsum 方程（见 4.2）。
5. 逐部件加载：
   - **embeddings**：`Embeddings::from_safetensors_loader(&loader)`（[gpt2/mod.rs:219](../../zkml/src/parser/llm/models/gpt2/mod.rs#L219)），读 `transformer.wte.weight`，shape `[50257, 768]`。
   - **全局位置编码**：`Positional::from_safetensors_loader`（[gpt2/mod.rs:225](../../zkml/src/parser/llm/models/gpt2/mod.rs#L225)），读 `transformer.wpe.weight`。GPT-2 是绝对位置。
   - **12 个 decoder block**：对每个 `i`，用前缀 `h.{i}.` 调 `GPT2Decoder::from_loader`（[gpt2/mod.rs:230-237](../../zkml/src/parser/llm/models/gpt2/mod.rs#L230-L237)）。
   - **最终 LayerNorm**（ln_f）：读 `ln_f.weight`/`ln_f.bias`，eps 来自 config（[gpt2/mod.rs:240-246](../../zkml/src/parser/llm/models/gpt2/mod.rs#L240-L246)）。
   - **最终投影**（LM head）：equation 复用 embeddings 矩阵 `X(se)@WE(ve)`（[gpt2/mod.rs:248-257](../../zkml/src/parser/llm/models/gpt2/mod.rs#L248-L257)）。GPT-2 **权重绑定**（tied embeddings），所以这里直接引用 `embeddings.mat`。
     - ⚠️ 关键细节：`proj_weights.key` 被改成 `{原key}_final_proj`（[gpt2/mod.rs:261](../../zkml/src/parser/llm/models/gpt2/mod.rs#L261)）。**为什么必要**：embeddings 矩阵在解析后被量化缩放，而 final_proj 不被缩放；两者是不同张量，commit 时若用同一个 key 会在 [context.rs:329](../../zkml/src/iop/context.rs#L329) 触发 `Found different MLE for polynomial ...` 断言。这就是你之前 gpt2 报那个 panic 的根因。
6. 把上述全部包成 `LLMIR::new(...)`（[gpt2/mod.rs:268-274](../../zkml/src/parser/llm/models/gpt2/mod.rs#L268-L274)），最后 `into_model` 构图。

---

## 3. 单层内部：GPT2Decoder & GPT2Attention 的权重形状

### 3.1 Decoder（一个 transformer block）

文件：[`src/parser/llm/models/gpt2/decoder/mod.rs`](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs)

- `GPT2Decoder::from_loader`（Safe 版）在 [decoder/mod.rs:83](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L83)：
  - `ln_1`（pre-attention LN）：读 `ln_1.weight/bias`，校验 shape `[768]`（[decoder/mod.rs:93-110](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L93-L110)）。
  - attention：`GPT2Attention::from_loader`（[decoder/mod.rs:114](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L114)）。
  - `ln_2`（pre-FFN LN）：读 `ln_2.weight/bias`（[decoder/mod.rs:117-131](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L117-L131)）。
  - FFN：读 `mlp.c_fc.weight/bias`（up，`[768, 3072]`）和 `mlp.c_proj.weight/bias`（down，`[3072, 768]`），激活函数是 `Gelu`（[decoder/mod.rs:145-171](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L145-L171)）。GPT-2 用的是 **Conv1D** 存储约定（`output = input @ W`，不转置），注释见 [decoder/mod.rs:134-144](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L134-L144)。

### 3.2 Attention（fused c_attn 拆分）

文件：[`src/parser/llm/models/gpt2/decoder/attention.rs`](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs)

- `GPT2Attention::from_loader`（Safe 版）在 [decoder/attention.rs:107](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L107)：
  - GPT-2 把 QKV 融合在一个 `attn.c_attn.weight` 里，shape `[768, 3*768]`（列 = `[Q|K|V]`）（[decoder/attention.rs:144-149](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L144-L149)）。
  - 手动把每行的列切成 Q/K/V 三段（[decoder/attention.rs:159-177](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L159-L177)），各 reshape 成 `[e, num_heads, head_size] = [768, 12, 64]`（[decoder/attention.rs:179-211](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L179-L211)）。
  - 同样拆 fused bias `attn.c_attn.bias`（[decoder/attention.rs:213-245](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L213-L245)）。
  - 输出投影 `attn.c_proj.weight` reshape 成 `[num_heads, head_size, e]`，`c_proj.bias` shape `[768]`（[decoder/attention.rs:250-278](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L250-L278)）。

⚠️ 这是**数据形状的最终形态**：Q/K/V 是 `[e,h,d]`，O 是 `[h,d,e]`。它能否跟 einsum 方程($"WQ(ehd):..."$)对上，取决于方程选的是不是 MHA —— 见 4.2。

---

## 4. 构图：LLMIR → Model 图

### 4.1 `LLMIR::into_model` —— 把部件接成 DAG

文件：[`src/parser/llm/mod.rs`](../../zkml/src/parser/llm/mod.rs)

`into_model` 在 [llm/mod.rs:134](../../zkml/src/parser/llm/mod.rs#L134)：

1. `Model::new_from_input_shapes(...)`：以输入 shape 建空图（[llm/mod.rs:139](../../zkml/src/parser/llm/mod.rs#L139)）。
2. 依次插入节点，用 `last_node_id` 串成「上一步输出 → 下一步输入」的链：
   - `Embeddings`（[llm/mod.rs:141-142](../../zkml/src/parser/llm/mod.rs#L141-L142)）
   - 可选 `Positional`（wpe，[llm/mod.rs:144-150](../../zkml/src/parser/llm/mod.rs#L144-L150)）
   - 循环每个 block 调 `add_to_model` 并把返回的新 id 接上（[llm/mod.rs:152-157](../../zkml/src/parser/llm/mod.rs#L152-L157)）
   - `final_norm`（ln_f，[llm/mod.rs:158](../../zkml/src/parser/llm/mod.rs#L158)）
   - `final_proj`（LM head，[llm/mod.rs:160-164](../../zkml/src/parser/llm/mod.rs#L160-L164)）
   - `Logits`（argmax，[llm/mod.rs:166-167](../../zkml/src/parser/llm/mod.rs#L166-L167)）；`automatic_output_labelling` 自动把无后继的节点标成模型输出（[llm/mod.rs:168](../../zkml/src/parser/llm/mod.rs#L168)）。
3. 把各部件 id 记进 `LLMMetadata`（[llm/mod.rs:169-177](../../zkml/src/parser/llm/mod.rs#L169-L177)）——后面 Driver 和上下文生成靠它定位关键节点。

### 4.2 Decoder 的构图逻辑（残差 + attention + 残差）

文件：[`src/parser/llm/models/gpt2/decoder/mod.rs`](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs)

`GPT2Decoder::add_to_model` 在 [decoder/mod.rs:35](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L35)：

1. `ln_1` 作为链上第一个节点（[decoder/mod.rs:55-56](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L55-L56)）。
2. attention 接在 ln_1 之后（[decoder/mod.rs:59-60](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L59-L60)），其内部会插入 QKV/query-key/output 三个 einsum（见 4.3）。
3. **残差 Add**：把 `ln_1` 的**输入**（上一 block 输出）和 attention 输出相加（[decoder/mod.rs:62-64](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L62-L64)）。数据流上：`x + Attn(LN1(x))`。
4. `ln_2` 接在 Add 之后（[decoder/mod.rs:66-67](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L66-L67)）。
5. FFN 接在 ln_2 之后（[decoder/mod.rs:69](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L69)），再过第二个残差 Add：`y + FFN(LN2(y))`（[decoder/mod.rs:70-72](../../zkml/src/parser/llm/models/gpt2/decoder/mod.rs#L70-L72)）。
6. 返回 block 的最终输出 node id 作为下一 block 的输入。

### 4.3 attention 内部的三个 einsum（数据流核心）

文件：[`src/parser/llm/transformer/attention_layer.rs`](../../zkml/src/parser/llm/transformer/attention_layer.rs)

这是「数据流视角」最关键的一段 —— 张量在这里真正流动：

**方程选择** `form_einsum_equations` 在 [attention_layer.rs:56](../../zkml/src/parser/llm/transformer/attention_layer.rs#L56)。它按 `heads_per_kv_head()` 分流：
- `== 1` → MHA（gpt2 走这条）
- `== total_heads` → MQA
- 否则 → GQA

**MHA 方程** `form_mha_einsum_equations` 在 [attention_layer.rs:262](../../zkml/src/parser/llm/transformer/attention_layer.rs#L262)：

```
X(se) @ WQ(ehd):WK(ehd):WV(ehd) -> Q(hsd):K(hsd):V(hsd)   // QKV 投影
Q(hqd) @ K(hsd) -> A(hqs)                                  // 注意力得分
A(hqs) @ V(hsd) -> O(hqd)                                  // 加权求和
O(hqd) @ WO(hde) -> Y(qe)                                  // 合并多头 → 输出
```

各个字母的含义（gpt2：s=seq, e=768, h=12, d=64）：
- `s` 序列、`q` 查询位置
- `e` 嵌入维（768）
- `h` head 数（12）、`d` head 维（64）

**QKV einsum 节点** `build_qkv_einsum` 在 [attention_layer.rs:84](../../zkml/src/parser/llm/transformer/attention_layer.rs#L84)。它把 4.2 里 attention 的 map（即 `form_mha_einsum_equations` 选中的方程）和 `attention_mechanism.qkv_tensors()`（GPT-2 返回的 wq/wk/wv + bias，[gpt2 attention.rs:68-76](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L68-L76)）组合，创建一个 `EinSum` 节点。

**GPT-2 特有接线** `insert_custom_logic` 在 [decoder/attention.rs:86](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L86)：给 QKV einsum 挂 K/V 缓存，并把 Q(0)、K(1) 两个输出直接接到 query-key 节点（[decoder/attention.rs:102](../../zkml/src/parser/llm/models/gpt2/decoder/attention.rs#L102)）。加权求和与输出投影 einsum 由通用 attention_layer 按方程建。

> 到这里，**权重信号流**（Q/K/V/O 四组矩阵）已作为常量张量嵌进图的四个 einsum 节点；**输入信号流**（token 序列）从 Embeddings 喂入，逐节点流动。

---

## 5. 量化 + Prove-able Driver

回到 [`llm.rs`](../../zkml/src/model/llm.rs)。

在 [llm.rs:212-224](../../zkml/src/model/llm.rs#L212-L224)，`driver.into_provable_llm(None)` 对 `Driver<f32>` 做「可证明化」：

- `into_provable_llm` 在 [llm.rs:175](../../zkml/src/model/llm.rs#L175)：
  1. 生成几个「代表输入」来标定量化尺度（[llm.rs:181-190](../../zkml/src/model/llm.rs#L181-L190)）。
  2. `to_quantized(self.model, conf)` 把 `Model<f32>` 转成 `Model<Element>`（每个数被缩放到有限域元素 + 逐步量化，[llm.rs:202](../../zkml/src/model/llm.rs#L202)）。
  3. 重要：**推理用的输入 shape 始终保持 `[1]`**（[llm.rs:204](../../zkml/src/model/llm.rs#L204)）——一条 token 一条 token 跑，靠 kv-cache 累积出整段 Trace；padding 只在证明层做（注释见 [llm.rs:461](../../zkml/src/model/llm.rs#L461)）。

现在 `driver` 已经是一个「张量每一步都能被 ZK 计算」的 Provable 模型。

---

## 6. 一次性上下文（setup）生成

在 [llm.rs:226-246](../../zkml/src/model/llm.rs#L226-L246)：
- 若 `--load-params setup`：从 `setup.pk`/`setup.vk` 反序列化 Prover/Verifier 上下文（[llm.rs:226-240](../../zkml/src/model/llm.rs#L226-L240)）。
- 否则调 `driver.context()` 现场生成（[llm.rs:241-245](../../zkml/src/model/llm.rs#L241-L245)）。
- 若 `--save-params setup`：把上下文序列化落盘（[llm.rs:248-263](../../zkml/src/model/llm.rs#L248-L263)）。

`Driver::context` 在 [llm.rs:698](../../zkml/src/model/llm.rs#L698)，真正干活的是 `self.model.generate_contexts()`（[llm.rs:709](../../zkml/src/model/llm.rs#L709)）。

`Model::generate_contexts` 在 [`src/iop/context.rs`](../../zkml/src/iop/context.rs) [context.rs:203](../../zkml/src/iop/context.rs#L203)：
1. 把所有输入 shape 扩展到 2 的幂（`next_power_of_two`，[context.rs:208-212](../../zkml/src/iop/context.rs#L208-L212)）——ZK 要求多项式长度是 2 的幂。
2. `build_global_context_data`（[context.rs:95](../../zkml/src/iop/context.rs#L95)）：**前向遍历整个图**（`try_map_forward`，[context.rs:118](../../zkml/src/iop/context.rs#L118)），对每个层：
   - 由「上层输出 shape」推导本层输出 shape，为每步确定输入/输出形状（`compute_layer_ctx`，[context.rs:311](../../zkml/src/iop/context.rs#L311)）。
   - 把所有**常量权重**转成 `DensePolynomial<F>` 收集进 `model_polys`（[context.rs:324-336](../../zkml/src/iop/context.rs#L324-L336)）。**如果同一个 `CommitmentId` 收到两份不同的 MLE，就在 [context.rs:329](../../zkml/src/iop/context.rs#L329) 触发那个 `debug_assert!("Found different MLE ...")`** —— 这就是权重 key 冲突的现场。
   - 记录各类型 lookup 表（[context.rs:141-147](../../zkml/src/iop/context.rs#L141-L147)+[context.rs:133](../../zkml/src/iop/context.rs#L133)），并让 `max_poly_len` 取每步多项式长度与表尺寸的最大值（[context.rs:149](../../zkml/src/iop/context.rs#L149)、[context.rs:169-172](../../zkml/src/iop/context.rs#L169-L172)）。
3. `GlobalCommitmentContext::new(...)` 生成承诺体系上下文：需要的多项式变量数 = `log2(max_poly_len)`（[context.rs:190-197](../../zkml/src/iop/context.rs#L190-L197)）。
4. `generate_contexts()` 返回 `ProverContext`（含模型上下文 + 承诺实现 + lookup）与 `VerifierContext`（承诺验证端，模型上下文不变）（[context.rs:216-230](../../zkml/src/iop/context.rs#L216-L230)）。

**为什么能重复用**：这个上下文只依赖「图的形状 + 权重承诺」，不依赖具体输入 → 一份 setup 可复用于任意输入序列（所以能 `--load-params` 跳过重复计算）。

---

## 7. 推理：产生 Trace —— 数据流「执行」阶段

> **本节目标**：把"推理怎么变成一份 `Trace<Element=i64>`"讲到底——**一个两条腿的阶段**：先自回归把 token 生出来（不记 Trace），再用整句重跑一版**全记录 Trace**。下面照 [llm.rs:265-302](../../zkml/src/model/llm.rs#L265-L302) 的调用，逐层下钻。

### 7.1 入口四连

回到 [`llm.rs`](../../zkml/src/model/llm.rs)：

| 调用 | 位置 | 干什么 |
|---|---|---|
| `driver.with_max_context(max_ctx)` | [llm.rs:272](../../zkml/src/model/llm.rs#L272) | 设定本会话上下文窗口（决定能跑多长） |
| `driver.random_sequence(user_prompt)` | [llm.rs:273](../../zkml/src/model/llm.rs#L273) | 生成随机 token 序列（≈ 你的 prompt） |
| `driver.tokens_to_tensor(...)` | [llm.rs:274](../../zkml/src/model/llm.rs#L274) | token `Vec` → 输入张量 |
| `driver.run_elements(input_tensor, &mut GenStore)` | [llm.rs:380](../../zkml/src/model/llm.rs#L380) | → **`Trace<Element>`** |

`GenStore`（来自 `tenstore` crate，经 runner 持有，[model/mod.rs:148-152](../../zkml/src/model/mod.rs#L148-L152)）是张量句柄的存储区：推理产生的中间张量不再按值存进图节点，而是按**句柄**指向 store，避免每次复制整块数据。

### 7.2 `run_elements` → `run_elements_with_runner`：两条腿

`run_elements`（[llm.rs:380](../../zkml/src/model/llm.rs#L380)）是薄壳，真身是 `run_elements_with_runner`（[llm.rs:417](../../zkml/src/model/llm.rs#L417)）。它分**两大步**：

```
run_elements_with_runner
 ├─ reset() 清缓存 / 校验长度 / 建输入句柄          (llm.rs:429-466)
 ├─ ▶【腿 A】自回归循环：for k = num_input..=max_window   (llm.rs:495-544)
 │    每轮: run_with_runner 前向(K个token)→ 取末 token logits → argmax 出新 token
 │          后台线程收集 token、监听 EOS；把新 token 拼回输入句柄，下一轮接续
 │          慢，逐 token；【不记录完整 Trace】
 └─ ▶【腿 B】整句重跑：run_with_split_nodes_info(full_sentence)   (llm.rs:570-572)
      一次前向跑完所有 token，所有节点输入/输出/辅助量【全落进一份 Trace<Element>】
```

**为什么是两条腿（关键，否则看不懂 Trace 哪来的）**：
- 腿 A 用**逐个 token** 跑，目的是把"下一个 token"一次一个地生成出来、拼成完整句子 `full_sentence`；这个阶段**不记录 Trace**，只收集生成的 token。
- 腿 B 用**整句**一次性前向（不逐 token），把每个节点的中间量都记录下来——**这份才是真正喂给 prove 的 `Trace<Element>`**。
- 一句话：**"生成句子"和"产生 Trace"是两次不同的前向**。腿 A 是推理本体，腿 B 是对腿 A 结果的"复跑+全录"。

### 7.3 腿 A：自回归循环（逐 token，不记录全量 Trace）

`run_elements_with_runner` 的核心循环在 [llm.rs:495](../../zkml/src/model/llm.rs#L495)：

1. **准备**：`self.model.reset()`（[llm.rs:429](../../zkml/src/model/llm.rs#L429)）清空 KV/softmax 缓存；校验 `num_input_tokens < context-1`（至少留一个位置生成）、`≤ max_window`、token 在词表内（[llm.rs:437-456](../../zkml/src/model/llm.rs#L437-L456)）。`num_input_tokens = full_sentence 初始长度`。
2. **后台收集线程**：开一个 `sync_channel` + 线程（[llm.rs:468-491](../../zkml/src/model/llm.rs#L468-L491)），逐 token 收下生成结果、凑 `generated_tokens`；一旦收到 `eos_token` 就置 `stop` 标志让主循环停。**最后 `generated_tokens.pop()` 弹掉最后一个 token**（[llm.rs:487](../../zkml/src/model/llm.rs#L487)）——因为最后一个 token 是"半吊子"状态，要在腿 B 用全量 padding 重生成一遍。
3. **主循环** `for unpadded_seq_len in num_input_tokens..=max_window`（[llm.rs:495](../../zkml/src/model/llm.rs#L495)）：
   - `stop` 若被 EOS 线程置起则 break（[llm.rs:496-502](../../zkml/src/model/llm.rs#L496-L502)）。
   - `self.model.run_with_runner(&mut runner, input_handles)`（[llm.rs:510](../../zkml/src/model/llm.rs#L510)）——**一次完整前向**，把当前 `[k]` 个 token 整张图跑一遍。
   - 取输出唯一张量，选中**末位 token** 的 logits（`index = unpadded_seq_len-1`，[llm.rs:530-537](../../zkml/src/model/llm.rs#L530-L537)），经 argmax 得到新 token。
   - `tx.send(last_token_tensor)` 交给收集线程（[llm.rs:539](../../zkml/src/model/llm.rs#L539)）；再把新 token 转成句柄 `input_handles`（[llm.rs:542-543](../../zkml/src/model/llm.rs#L542-L543)），下一轮 shape 变 `[k+1]`——**这就是自回归循环**。
4. **收尾**：`drop(tx)` 关发送端 → `handle.join()` 取回收集线程攒下的全部 `generated_tokens`（[llm.rs:548-554](../../zkml/src/model/llm.rs#L548-L554)），拼进 `full_sentence`。

> **数据形状**：每次 `run_with_runner` 输入 shape 是 `[num_input_tokens], [k+1], [k+2], ...` **逐步增长的未 padding 张量**。量化模型的"input shape 始终保持 `[1]`"（[§5](../../zkml/../docs/zkml_bootcamp/gpt2_导读_数据流.md#5-量化--prove-able-driver)）指**推理单元**，腿 A/B 的实际句长随 token 线性增长，padding 只在证明层做（[llm.rs:461,555](../../zkml/src/model/llm.rs#L461)）。

### 7.4 腿 B：整句重跑，产出 `Trace<Element>`（重点）

`run_elements_with_runner` 的最后一段（[llm.rs:556-581](../../zkml/src/model/llm.rs#L556-L581)）：

1. 用拼好的 `full_sentence` 重建一个**完整张量** `Tensor::new(Shape::new([len]), full_sentence)`（[llm.rs:556](../../zkml/src/model/llm.rs#L556)）——**这次不再逐 token，把所有 token 一起喂**。
2. `self.model.reset_selective(&[SOFTMAX_LAYER])`（[llm.rs:560-561](../../zkml/src/model/llm.rs#L560-L561)）：只清 softmax 的缓存、**其余 KV 缓存保留**，让最后这一前向能复用前面算好的状态、又不被 softmax 旧序列干扰——这句正是"腿 B 是对腿 A 的复跑"的接线。
3. `self.model.run_with_split_nodes_info(vec![tensor], store, split_node_info)`（[llm.rs:570-572](../../zkml/src/model/llm.rs#L570-L572)）→ 返回 **`Trace<Element> = 整段推理每个节点中间量的记录**。这就是 prove 要证明的东西。

#### 7.4.1 `run_with_split_nodes_info` 内部：一次前向 + 逐节点落 Trace

`run_with_split_nodes_info` 在 [model/mod.rs:1587](../../zkml/src/model/mod.rs#L1587)，把 runner 包成**四层洋葱**（[model/mod.rs:1604-1614](../../zkml/src/model/mod.rs#L1604-L1614)）：

```
BaseRunner ──▶ StoreRunner ──▶ TraceRunner ──▶ HandleLifetimeRunner
  真正计算      后台线程落盘     建/维护 trace    管句柄生命周期
```

- `run_with_runner`（[model/mod.rs:1552](../../zkml/src/model/mod.rs#L1552)）按 `graph.forward_inners()` **拓扑序**遍历每个内节点，逐个调 `runner.run_layer(node_id, graph, layer, &())`（[model/mod.rs:1567-1582](../../zkml/src/model/mod.rs#L1567-L1582)）——一次前向就是"顺着图的每条边把中间值算过去"。
- `StoreRunner`（`DebugRunner::run_layer` 会把它打包，[model/mod.rs:419-450](../../zkml/src/model/mod.rs#L419-L450)）把 `node_id` + `node_inputs` + `node_outputs` + `proving_data` 组成 `LayerRun` 消息发到后台线程。
- 后台线程（`trace.rs` + [model/mod.rs:368-372](../../zkml/src/model/mod.rs#L368-L372)）为**每个节点**建一个 `Step`：

```rust
let new_step = Step {
    node_inputs,                     // 该节点输入端口收到的张量（i64）
    node_outputs: NodeOut::new(node_outputs, proving_data),  // 输出 + 层内辅助量
};
trace.new_step(node_id, new_step);   // Trace.steps: HashMap<NodeId, Step>
```

- `Trace<D>`（[trace.rs:29](../../zkml/src/model/trace.rs#L29)）= `steps: HashMap<NodeId, Step>`；`Element = i64`（[lib.rs:103](../../zkml/src/lib.rs#L103)）。**图有多少个 node，Trace 就有多少个 Step，一一对应**。

#### 7.4.2 每条边 / 每个节点 record 什么（数据流真正被冻结下来）

| 节点类型 | Step 里的 `node_outputs` | `proving_data`（层内辅助量，证明器要 check） |
|---|---|---|
| einsum（QKV/score/value/out/c_fc/c_proj） | 输出稠密张量（i64） | —（`ProvingHandle::None`） |
| LayerNorm / RMSNorm | 归一化结果 | **mean / std**（`LayerNormHandle`），RMS 存 normalisation |
| Softmax（含 mask） | 概率分数 | **shift**（`SoftmaxHandle`） |
| GELU / 残差 Add | 输出张量 | — |
| Embeddings | 查表拼接 | — |
| ArgMax（logits） | 末 token 概率分布 | **max_values**（`ArgmaxHandle`） |

> 这些 `proving_data` 不是张量流本身，而是**层内用于核对关系的辅助中间量**——证明器在 [§8](../../zkml/../docs/zkml_bootcamp/gpt2_导读_数据流.md#8-证明prover) 会连同主张量一起检查（对应 [model/mod.rs:331-367](../../zkml/src/model/mod.rs#L331-L367) 的转换）。

#### 7.4.3 腿 B 的另一件事：输出移位校验

腿 B 末尾有一段**自检**（[llm.rs:573-581](../../zkml/src/model/llm.rs#L573-L581)）：

```rust
for i in num_input_tokens..full_sentence.len() {
    assert_eq!(trace.outputs()[0].data()[i-1], trace.inputs()[0].data()[i], ...);
}
```

- 因为推理是**自回归右移一位**的（第 i 个位置的输出是"下一个 token 的分布"），所以 `Trace` 输出末尾应该恰好**比输入右移一位**。这段 `assert_eq` 就是核对"复跑的这次前向，输出确实等于右移后的输入"，**保证腿 B 的 Trace 与腿 A 的生成结果一致**。
- 若 `num_input_tokens < max_window` 还会再校验一次 argsize 完整性（[llm.rs:586-...](../../zkml/src/model/llm.rs#L586)）。

> **收拢**：`Trace<Element>` 就是「腿 A 自回归定出的完整句子 × 腿 B 整句前向」后，**每个 node 的输入/输出/辅助量以 i64 全量冻结下来的一张大表**。它和图的拓扑一一对应，prove 一上来就消费它（域化 → 承诺 → 逆拓扑 sumcheck）。推理的**类型和值**都已固化：`i64` 定点数、由量化权重真算出来、存进 store。

---

## 8. 证明：Prover

回到 [`llm.rs`](../../zkml/src/model/llm.rs)：

- `driver.prove(&prover_ctx, trace)` 在 [llm.rs:317](../../zkml/src/model/llm.rs#L317) 调用。
- `Driver::prove` 在 [llm.rs:774](../../zkml/src/model/llm.rs#L774)：
  - 选默认 chunking 策略（[llm.rs:788](../../zkml/src/model/llm.rs#L788)）。
  - 调 `chunked_prove_local`，chunk 数 = 1（顺序执行，[llm.rs:789-795](../../zkml/src/model/llm.rs#L789-L795)）。
- `chunked_prove_local` 在 [llm.rs:721](../../zkml/src/model/llm.rs#L721)，核心是 `Prover::chunked_prove_local::<...>`（[llm.rs:736](../../zkml/src/model/llm.rs#L736)）。

`Prover`（[`src/iop/prover_graph.rs`](../../zkml/src/iop/prover_graph.rs)）是怎么消费 Trace 的：

1. 从 `Model` 图 + 每个节点在 Trace 里的输入/输出值，为每个节点构造一个 `ProverGraphNode`（它知道本层 weight 承诺、输入/输出 witness）。
2. **按逆拓扑序遍历**（证明从最后一层往回走）：每层做
   - 接收来自下一层的「挑战」（用于 sumcheck 的各项求值点）
   - 用本层的输入/输出/权重做一个 **sumcheck/小曲线证明片段**，承诺相关多项式并回传挑战
   - 产出一个「该步的 Proof 片段」。
3. 把所有片段聚合到一颗最底层的多项式，最终由 PCS（这里是 `HyperKZG`，[llm.rs:37](../../zkml/src/bin/bench/llm.rs#L37)）产出最终的 `Proof`。

> 这也解释了你之前看 `split_in_chunks`（[`iop/context.rs:53`](../../zkml/src/iop/context.rs#L53)、`--distributed`）的意义：把图切成若干 chunk，可分发到多个 worker 并行证明。顺序版 chunk=1 就是串起来跑。

**证明大小**在 [llm.rs:336](../../zkml/src/bin/bench/llm.rs#L336) 记录：`rmp_serde::to_vec(&proof)?.len()`。

---

## 9. 验证：Verifier

回到 [`llm.rs`](../../zkml/src/model/llm.rs)：

- `verifier_ctx.with_max_context(max_ctx)`（[llm.rs:339](../../zkml/src/model/llm.rs#L339)）
- `verifier_ctx.verify(proof, user_tokens, io)`（[llm.rs:341-343](../../zkml/src/model/llm.rs#L341-L343)），失败会 panic → 输出 `invalid proof`。
- `LLMVerifierContext::verify` 在 [llm.rs:799](../../zkml/src/model/llm.rs#L799)：
  - `io`（`driver.prove` 返回的 `IO<F>`）里带有模型输入 token 和期望输出 logits 的公共信息（[llm.rs:820-823](../../zkml/src/model/llm.rs#L820-L823)）。
  - 校验输出张量数量等于 1（[llm.rs:820-822](../../zkml/src/model/llm.rs#L820-L822)）。
  - 用 `verifier_ctx` 里存的（模型权重承诺 + 输入/输出承诺）跑一遍 HyperKZG 验证与 sumcheck 校验，证明「存在一条输入使模型输出 = io 里的 logits」。
- 最终 `measure::to_csv(&args.bench)`（[llm.rs:357](../../zkml/src/bin/bench/llm.rs#L357)）把各阶段耗时写进 `bench.csv`。

---

## 10. 数据流「快照」：一条 token 从进到出

把整条通路收成一句话：

1. **权重**：`model.safetensors` → `FileTensorLoader` → 各 `Layer` 的常量 `Tensor` → （量化后）图节点的常量张量。
2. **上下文**：图结构 → 每步 shape → 每个常量张量变成一个可被 sumcheck 引用的多项式（`DensePolynomial<F>`），连同 PCS 承诺一起封进 `ProverContext`/`VerifierContext`。
3. **输入**：token 序列 → `Trace<Element>`（自回归逐 token，每层中间值全记录）。
4. **证明**：`Trace` + `ProverContext` → 逆序遍历图，逐层 sumcheck + 最终 HyperKZG 承诺 → `Proof`。
5. **验证**：`Proof` + `VerifierContext` + 公共 io → 校验整个链条成立。

三份对象对应关系：
| 一次性（setup） | 每次运行 |
|---|---|
| `ProverContext`（[context.rs:36](../../zkml/src/iop/context.rs#L36)） | `Trace<Element>` |
| `VerifierContext`（[context.rs:73](../../zkml/src/iop/context.rs#L73)） | `Proof` + `IO<F>` |

---

## 11. ZK 的输入数据从哪来（witness 专讲）

> 这一节回答一个反复出现的问题：**证明器到底要证明什么，这些数据从哪来、是什么类型、怎么被消费**。只讲事实，不给方案。

### 11.1 一句话

**ZK 证明的输入分三类：常量（模型权重）、witness（推理每层的输入/输出值）、挑战（sumcheck 逐轮随机点）。真正"要证明"的是 witness——它是把权重跑一次推理时，逐层真算出来的值被原样存下来的完整轨迹。**

| 输入 | 是谁 | 来源 | 是否复用 |
|---|---|---|---|
| 模型权重 | 公共常量 | `model_cache/*/model.safetensors` | 一次生成（setup，可 `--save-params`） |
| **witness** | 证明的数据 | `run_elements` 推理过程的每层输入/输出 | **每次不同** |
| 挑战 r | 协议量 | sumcheck 过程性随机派生 | 每次不同 |

### 11.2 witness 是什么

- **类型**：推理阶段 `Tensor<Element>`，`pub type Element = i64`（[lib.rs:103](../../zkml/src/lib.rs#L103)）。**不是 BN254、不是 u64，是量化后的有符号 64 位整数**。
- **结构**：`Trace<D>`（[trace.rs:29](../../zkml/src/model/trace.rs#L29)）的 `steps: HashMap<NodeId, Step>`。每个 `Step`（[trace.rs:388](../../zkml/src/model/trace.rs#L388)）= `node_inputs`（该节点输入张量）+ `node_outputs`（输出张量，含 `proving_data`）。
- **proving_data 是层内辅助量**（[model/mod.rs:331-367](../../zkml/src/model/mod.rs#L331-L367)）：LayerNorm 存 mean/std、RMSNorm 存 normalisation、Softmax 存 shift、ArgMax 存 max_values。这些是证明器要 check 的额外中间量。

### 11.3 witness 怎么算出来（i64 是怎么来的）

**先量化权重，再逐 token 推理。**

1. **量化**：f32 权重 ÷ scale 取整到 i64（[quantization/mod.rs:144](quantization/mod.rs#L144) `(*value / self.scale()).round_ties_even() as Element`），scale 由代表输入统计得出。
2. **推理**（[run_elements_with_runner](model/llm.rs#L417)）：逐 token 自回归 `for unpadded_seq_len in ...max_window`，每轮 `run_with_runner` 把整张图跑一遍（[llm.rs:495-511](model/llm.rs#L495-L511)）。每一层 forward 把 input/output 真算出来，由 `DebugRunner` 发 `LayerRun` 消息（[model/mod.rs:442](model/mod.rs#L442)），`StoreRunner` 后台线程写入 `GenStore`（[model/mod.rs:583](model/mod.rs#L583)）。**推理在 i64 定点域上跑，算出的值是什么 Trace 就存什么。**

### 11.4 证明时：类型变换链（i64 → Fr → MLE）

**证明器和推理用的不是同一类型。** prover 从 trace 加载时（[prover.rs:703-708](prover.rs#L703)）：

```
Trace<Element=i64>
   │  output_tensors() → Tensor<i64>
   │  to_field()        → Tensor<ark_bn254::Fr>（i64 直接装箱域元素）
   │  pad_next_power_of_two() → 长度补到 2^k
   ▼
DensePolynomial<Fr>（把值当布尔超立方体上的求值表）
```

- **`i64 → Fr`**：`F::from(*self)`（[quantization/mod.rs:263-267](quantization/mod.rs#L263)），ark-ff 的 `From<i64>`，有符号整数 → 模 p 的 Montgomery 域元素，负数取 mod——精确映射，无浮点误差。
- **`into_mle`**（dp-crypto）：`Vec<Fr>` → `DensePolynomial::new`（[dp-crypto sumcheck/mod.rs:30](dp-crypto/sumcheck/mod.rs#L30)），**不做 FFT、不改顺序**，直接把值当超立方体求值表存进 `z: SmartSlice<F>`；长度必须 2 的幂，所以前面要 `pad_next_power_of_two` 补零。

### 11.5 sumcheck 和 MSM 的输入

- **sumcheck**：输入 `DensePolynomial<Fr>`（MLE）+ 折叠点 r，在 **Fr 域**上做部分求值 / EQ-β 折叠 / 归约。全是 Fr，不是 i64、不是 G1。
- **MSM（承诺）**：把 MLE 的 `Fr` 标量 × 固定的 **BN254 G1 基点**（SRS）做 Pippenger 批量标量乘，输出 `G1Affine` 承诺点。MSM 输入 = `Fr` 标量 + G1 基点，两者缺一不可。

### 11.6 证明器怎么消费 witness（顺序与并行）

- **先全量、后证明**：`run_elements` 把**整段推理**（所有 token × 所有层）的 Trace 全部存好，之后才调 `prove`。bench 是顺序的（[llm.rs:298](../../zkml/src/bin/bench/llm.rs#L298) → [llm.rs:317](../../zkml/src/bin/bench/llm.rs#L317)）。**没有"生成一个 token 证明一个"，也不并行**。
- **为什么逆拓扑序**：sumcheck 的随机挑战从**深层（输出侧）往浅层（输入侧）**流。每层 sumcheck 要求值点依赖下层刚产出的挑战，所以证明器只能逆数据流、顺挑战流走（[prover.rs:671](../../zkml/src/iop/prover.rs#L671) `backward_iter`）。不是性能选择，是协议顺序。
- **是否必须等推理全部完成**：**必须**。前层的 sumcheck 定义在尾层随机挑战之上，而尾层挑战要等证到尾层才产生，链路天然反向串行依赖。`split_in_chunks` 只把图切成多个子图并行证明，不改变"token 维度推理先全跑完"的前提。

### 11.7 一张图：值从推理到证明的类型流

```
推 理（定点 i64）                         证 明（域 Fr + 曲线 G1）
───────────────────────────────   ──────────────────────────────
权重 f32 ──÷scale取整──▶ i64         Trace<Element = i64>
输入 token ──▶ forward 逐 token    to_field() ──▶ Fr
   │                              pad_next_power_of_two ──▶ 长度 2^k
   ▼                              into_mle() ──▶ DensePolynomial<Fr>
Tensor<Element=i64>（每层 in/out）     │
   │（run_elements 全量存）           ├─▶ sumcheck: Fr 乘加/折叠
   ▼                              └─▶ MSM: Fr 标量 × G1 基点 ─▶ 承诺点
Trace<Element=i64>（全量 witness）
```

---

## 附：排查线索速查（对照你踩过的坑）

- **`Found different MLE for polynomial wte.weight`**（[context.rs:329](../../zkml/src/iop/context.rs#L329)）→ 命中 2.2 第 5 点 / 4.3 的 key 冲突；gpt2 的修复在 [gpt2/mod.rs:261](../../zkml/src/parser/llm/models/gpt2/mod.rs#L261)（`_final_proj` 改名）。
- **某个 einsum 报 `Mismatched dimension sizes for axis ...`**（`src/layers/einsum/axis.rs`）→ 检查 4.2 的方程选择（MHA/MQA/GQA）是否和 3.x 里权重的 reshape 形状一致。形状对应表：
  - MHA(`hpg=1`)：WQ/WK/WV `[e,h,d]`，WO `[h,d,e]`
  - MQA(`hpg==heads`)：WQ `[e,h,d]`，WK/WV `[e,d]`，WO `[h,d,e]`
  - GQA(默认)：WQ `[e,g,h,d]`，WK/WV `[e,g,d]`，WO 需要按头做置换
- **`rope_theta not found` / 某 config 字段缺失**：若这是可选默认值（如 rope base = 10000），用 `config.get::<f32,_>("xxx").unwrap_or(default)` 兜底（参见 llama2 attention 里的写法）。
- **`Connection reset` / 加载报下载**：缓存目录是 CWD 相关的 `model_cache/`，确认从**同一目录**运行；离线环境请把模型预置进对应 CWD 的 `model_cache/<repo_id>/`。
