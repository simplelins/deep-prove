# DeepProve：可验证的端到端大语言模型推理
# DeepProve: Verifiable End-to-End Large Language Model Inference

**作者：** Nicolas Gailly¹，Ismael Hishon-Rezaizadeh¹，Tianyi Liu¹,²,†（刘天逸），Nicholas Mainardi¹,†，Dimitrios Papadopoulos³，Charalampos Papamanthou¹,⁴，Christodoulos Pappas³，Shravan Srinivasan¹,†，Zack Youell¹,†，Yupeng Zhang¹,²*†（张宇鹏）

> **英语原文：** Nicolas Gailly¹, Ismael Hishon-Rezaizadeh¹, Tianyi Liu¹,²,†, Nicholas Mainardi¹,†, Dimitrios Papadopoulos³, Charalampos Papamanthou¹,⁴, Christodoulos Pappas³, Shravan Srinivasan¹,†, Zack Youell¹,†, and Yupeng Zhang¹,²∗†

**单位：** ¹ Lagrange Labs；² 伊利诺伊大学厄巴纳-香槟分校（UIUC）；³ 香港科技大学；⁴ 耶鲁大学

> **英语原文：** ¹ Lagrange Labs; ² University of Illinois Urbana-Champaign; ³ Hong Kong University of Science & Technology; ⁴ Yale University

> 注：\* 表示作者按字母顺序排列；† 表示相关工作在 Lagrange Labs 任职期间完成。 (\* Authors are listed alphabetically. † Work done while affiliated with Lagrange Labs.)

---

## 摘要

大语言模型（Large Language Models, LLMs）是前沿的深度学习系统，通过从海量训练数据中学习模式，预测序列中的下一个词以生成类人的文本并回答查询，在广泛的 AI 服务中取得了显著成功。它们拥有无数的实际应用，包括聊天机器人 [Ope25]、编程辅助 [ea21]、医疗健康 [MS25]、搜索引擎、国防 [Loc24] 以及更多领域 [MMN⁺24]。这种在众多关键领域中的广泛适用性，引发了如何维护 LLM 推理结果完整性的问题，因为推理结果可能影响关键的决策步骤。

> **English:**
> Large Language Models (LLMs) are frontier deep learning systems that have achieved remarkable success across a wide range of AI services. However, their substantial computational and memory requirements make them difficult to deploy and run on local hardware. Due to these resource requirements, users often rely on untrusted cloud infrastructure providers to perform model inference. However, outsourcing introduces the challenge of verifying that the returned output is the genuine result of the specified model. In this work, we present DeepProve, the first system to enable efficient end-to-end verification of full LLM inference (i.e., for all generated tokens of a prompt) on untrusted cloud servers using zero-knowledge proofs (ZKPs). In contrast, prior work either provides only a proof-of-concept partial implementation for a single token (zkGPT, USENIX'25), or focuses exclusively on specific components of the inference pipeline, such as Softmax (zkLLM, CCS'24).

> DeepProve achieves end-to-end verification by certifying the correctness of the output sequence rather than encoding the expensive inference computation in-circuit, an approach that would require either circuit size quadratic in the sequence length or costly in-circuit modelling of RAM operations. The core building blocks of DeepProve are sum-check protocol and lookup arguments, which enable efficient proof of correctness of all operators needed for GPT-2 and Gemma 3, such as multi-head attention and layer normalization for GPT-2, and grouped-query attention, root mean square normalization, and rotary positional embeddings for Gemma 3. Our evaluation shows that DeepProve can prove inference of GPT-2 and Gemma 3 at approximately 174 and 86 tokens per minute, respectively, which is 20−60× faster than the state of the art, without any significant loss in accuracy. Verification takes only 1 to 3.7 seconds. By distributing proof computation across multiple nodes, DeepProve can further improve the prover time while reducing the memory requirements for individual machines. With distributed proving, DeepProve can scale the throughput to 1855 tokens per minute. Our work represents the first full system for end-to-end LLM inference verification, thus paving the way for secure and trustworthy AI services.

（注：PDF 中摘要为双栏排版，右侧栏为引言内容，已在引言部分完整展开，此处不再重复。）

---

## 1 引言

大语言模型是基于传奇的 Transformer 架构 [VSP⁺17] 的强大深度学习模型，通过从海量训练数据中学习模式，预测序列中的下一个词以生成类人的文本并回答查询。它们找到了无数的实际应用，包括聊天机器人 [Ope25]、编程辅助 [ea21]、医疗健康 [MS25]、搜索引擎、国防 [Loc24] 以及更多领域 [MMN⁺24]。这种在众多关键领域中的广泛适用性，引发了如何维护 LLM 推理结果完整性的问题，因为它可能影响关键的决策环节。

> **English:**
> Large Language Models (LLMs) are powerful deep learning models that are based on the fabled transformer architecture [VSP+ 17], trained by predicting the next word in a sequence to generate human-like text and answer queries by learning patterns from vast amounts of training data. They have found countless real-world applications including chatbots [Ope25], programming assistance [ea21], healthcare [MS25], search engines, defense [Loc24], and many more [MMN+ 24]. This wide applicability across numerous sectors of critical importance raises the question of preserving the integrity of the result of LLM inference, as it can potentially affect crucial decision-making steps.

考虑以下基于 AI 的服务：

> **English:** For example, consider the following AI-based services:

- **高端企业支持：** 企业通常付费访问先进的 LLM 能力用于客户支持机器人。然而，AI 提供商可能返回来自较弱模型的响应以削减成本，降低承诺的服务质量 [QSL⁺25, Sal23, Com24, Com23]。
- **编程助手：** 这些系统处理编写代码、编辑文件和执行命令行指令等操作 [Git26]，因此确保这些动作源自受信任的预期 LLM 而非恶意行为者至关重要，因为恶意代码可能被用户机器上的 IDE 无意执行。
- **政府采购与服务：** 主权国家（例如参见 [Wik25a]）正在探索部署 AI 系统，以减少采购腐败并增强公共服务透明度。在这种情况下，确保决策和行动源自经授权的 AI 系统而非潜在恶意行为者至关重要。

> **English:**
> • Premium enterprise support: Enterprises often pay a premium to access advanced LLM capabilities for customer support bots. However, AI providers could return responses from weaker models to cut costs, degrading the promised service quality [QSL+ 25, Sal23, Com24, Com23].
> • Programming assistants: These systems handle operations such as writing code, editing files, and executing command-line instructions [Git26], thus making it essential to ensure that the actions originate from the trusted intended LLM rather than a malicious actor, as malicious code can be unwittingly executed by the integrated development environment (IDE) on the user's machine.
> • Government procurement and services: Sovereign states (e.g., see [Wik25a]) are exploring deploying AI systems with the intention to reduce corruption in procurements and enhance transparency in public services. In this case, ensuring that decisions and actions originate from the authorized AI system, rather than from a potentially malicious actor, is essential.

同时，LLM 推理非常消耗资源，需要大量的计算和内存资源 [YJK⁺22]。因此，AI 服务提供商可能希望将推理任务外包给外部服务商。在这种情况下，同样需要确保返回结果的正确性，因为服务商可能有动机篡改它们。

> **English:**
> At the same time, LLM inference is highly resource-intensive, requiring substantial computational and memory resources [YJK+ 22]. As a result, AI service providers may themselves wish to outsource the task of inference to external service providers. In this case, again, one needs to ensure the correctness of returned results, as the service providers may be motivated to alter them.

上述背景强烈地推动了对深度学习推理结果验证的需求（例如 [LXZ21, SLZ24, QSL⁺25, CTK25]），即确保返回的推理结果是忠实执行模型的结果，未偏离计算过程。

> **English:**
> The above has strongly motivated the need to verify deep learning inference results in the literature (e.g., [LXZ21, SLZ24, QSL+ 25, CTK25]), i.e., to ensure that the returned inference outcome is the result of faithfully executing the model without deviating from the computation.

SNARKs [GGPR13, BSCTV14, GKR08, CMT12] 是强大的密码学原语，允许证明者生成一个小巧且易于验证的证明，说服验证者某个计算的正确性。通常通过将计算表示为算术电路，其正确求值简化为检查一组数值约束。已有大量工作将 SNARK 应用于保护 ML 任务的完整性，例如 MLP、CNN、决策树及其他深度学习模型的训练和推理（例如 [APKP24, GGJ⁺23, BFGV⁺23, LKKO24, LXZ21, LWQ⁺24, KHSS22, CCYY25, MY25, ZFZS20, PP24, zko24, TGM⁺25, ea25a]）。

> **English:**
> Succinct non-interactive arguments of knowledge (SNARKs) [GGPR13, BSCTV14, GKR08, CMT12] are powerful cryptographic primitives that allow a prover to convince a verifier about the correctness of a computation by producing a small and easy-to-verify proof. This is usually done by representing the computation as an arithmetic circuit whose correct evaluation can be reduced to checking a set of numerical constraints. There has been a vast line of works that apply SNARKs to protect the integrity of ML tasks such as training and inference of multilayer perceptron (MLP), convolutional neural network (CNN), decision trees, and other deep learning models (e.g., [APKP24, GGJ+ 23, BFGV+ 23, LKKO24, LXZ21, LWQ+ 24, KHSS22, CCYY25, MY25, ZFZS20, PP24, zko24, TGM+ 25, ea25a]).

少数近期工作 [SLZ24, QSL⁺25, CTK25] 专门致力于构建基于 SNARK 的协议来验证 LLM 推理。不幸的是，尽管它们取得了相当大的进展，但都存在一些关键局限。首先，它们只专注于证明单 token 推理的正确性，没有解释如何扩展到多 token 的现实场景。其次，现有工作要么只对 LLM 的特定部分提出高效证明方案（如 zkLLM [SLZ24] 为高效证明 Softmax 层提出协议，zkGPT [QSL⁺25] 为高效证明矩阵乘法提出协议）并省略其他层的细节；要么使用黑盒证明系统构建端到端系统，从而在实现上效率低下且极其耗费内存（如 [CTK25] 需要 4 TB RAM 的机器）。最后，迄今最高效的方案 [SLZ24, QSL⁺25] 只提供了省略证明者具体组成部分的概念验证实现。

> **English:**
> A smaller number of recent works [SLZ24, QSL+ 25, CTK25] have specifically focused on building SNARK-based protocols for verifying LLM inference. Unfortunately, while these prior works have made considerable progress towards this goal, they all suffer from some crucial limitations. First, they only focus on proving the correctness of a single-token inference and do not explain how to extend to the realistic scenario of multiple tokens. As we discuss next, this is not trivial and may lead to prohibitive prover overheads. Furthermore, existing works either focus on proposing efficient provers only for a specific part of LLMs (e.g., zkLLM [SLZ24] proposes a protocol for efficiently proving a Softmax layer, while zkGPT [QSL+ 25] a protocol for efficiently proving matrix multiplications) and omit details on other layers, or construct an end-to-end system using black-box proving systems thus becoming concretely inefficient and massively memory-consuming (e.g., the experimental evaluation of [CTK25] reports results for a machine with 4 TB RAM). Finally, the most efficient schemes to date [SLZ24, QSL+ 25] only offer proof-of-concept implementations which omit specific parts of the prover.

虽然这些结果是对我们目标的必要一步，但专注于单个构建模块或推理过程的一部分，会掩盖为整个 LLM 推理过程构建端到端可验证系统时出现的若干关键挑战——解决这些挑战并构建一个高效的端到端系统，将是本工作的重点。

> **English:**
> While their results are a necessary step towards our goal, focusing on individual building blocks or parts of the inference process hides several key challenges that arise when trying to build an end-to-end verifiable system for the entire LLM inference process in real-world LLMs–addressing these and building an efficient end-to-end system will be the focus of this work.

---

### 1.1 端到端证明中的挑战

在这里，我们讨论为了成功构建一个完备的、可验证的 LLM 推理系统必须解决的一些关键挑战，涉及可扩展性、效率和准确性。

> **English:**
> Here we discuss some of the key challenges that need to be addressed to successfully build a full-fledged system for verifiable LLM inference, related to scalability, efficiency, and accuracy.

**朴素推理的二次方爆炸（Quadratic blowup）。** LLM 推理的一个关键部分是自注意力机制 [VSP⁺17]，它在生成每个新 token 时会重新计算序列中所有先前 token 的注意力分数，使模型能够动态关注输入中最相关的部分。“朴素”计算这需要与输入序列长度呈二次方的时间。现有 LLM 系统通过用内存换取计算来克服这种二次方爆炸，即键值（Key-Value, KV）缓存 [Wik25c]。在 KV 缓存中，推理算法将名为 keys 和 values 的大张量存储在内存中，并在生成后续 token 时复用它们，而不重新计算所有先前 token 的注意力分数。

> **English:**
> A key part of LLM inference is the self-attention mechanism [VSP+ 17], which recomputes the attention scores over all previous tokens in the sequence, when generating each new token, enabling the model to dynamically focus on the most relevant parts of the input. Computed "naively" this would require time quadratic in the length of the input sequence. Existing LLM systems overcome this quadratic blowup by trading computation for memory. This approach is known as Key-Value (KV) caching [Wik25c]. In KV-caching, the inference algorithm stores the large tensors called keys and values in memory and reuses them for generating future tokens without recomputing the attention scores over all previous tokens.

当涉及证明 LLM 推理时，对于长度为 t 的序列遵循上述朴素方法，假设一个用于证明单个 token 正确性的专用电路大小为 |C|，将导致证明者时间为 O(t²|C|)。对于长 token 序列，这个开销令人望而却步，从而限制了任何基于该方法的证明系统的可扩展性。我们强调，现有可验证 LLM 推理的工作并未解决此问题，因为它们只专注于证明推理过程的某些部分（例如单 token 推理 [QSL⁺25]）。

> **English:**
> When it comes to proving LLM inference, following the naive approach above for a sequence of length t and assuming a specialized circuit for proving the correctness of a single token is of size |C|, would lead to prover time O(t²|C|). This is prohibitively large for long token sequences, thus limiting the scalability of any proof system based on this approach. We stress that prior works in verifiable LLM inference do not address this problem as they focus on proving only parts of the inference process (e.g., single-token inference [QSL+ 25]).

为克服此问题，人们可以尝试在 SNARK 内部模拟 KV 缓存技术 [Wik25c]。虽然这可能带来渐近上更快的证明者，但这仍然是一个不理想的方案。这是因为，要实现 KV 缓存，证明者必须在 SNARK 电路内模拟读写 RAM 操作。这会复杂化证明者的逻辑，即便使用最先进的 RAM 模拟协议（例如通过查找论证 lookup arguments），也会导致巨大的具体开销，因为这类协议要求证明者承诺额外的辅助数据，且这些数据与读/写次数呈线性关系。

> **English:**
> To overcome that issue, one could try to emulate KV-caching [Wik25c] techniques within the SNARK. While this may lead to asymptotically faster provers, it is still an undesirable solution. This is because, to realize KV-caching, the prover has to emulate read/write RAM operations within the SNARK circuit. This complicates the prover's logic and leads to large concrete overheads even when using state-of-the-art protocols for emulating RAM (e.g., via lookup arguments) as they require the prover to commit to additional auxiliary data that scales linearly to the number of reads/writes.

**纯整数推理的需求。** 大多数现有 SNARK 构造在有限域上工作。相比之下，LLM 计算在实数上定义，并使用浮点运算实现。弥合这一差距并非易事，可能引入显著的效率低下和/或潜在的精度损失。这在 SNARK 文献中是一个众所周知的难题，特别是当证明 ML 任务时 [HCL⁺24, SSPP25, RWB⁺25]。先前工作表明，在 SNARK 系统内模拟浮点运算仍相当不现实 [GJJZ22, EZC⁺25]，因此大多数现有工作 [QSL⁺25, CTK25, SLZ24, zko24, HCL⁺24, RWB⁺25] 通过在给定域上进行量化（例如 8 位）采用定点运算。然而，某些输入如果过小可能无法被正确表示为定点，而相反地，过大的输入可能使值溢出不合适。由于查找表范围需要预先固定，某些输入可能根本无法被证明。有趣的是，独立于密码学的完整性，深度学习从业者早已为了减小内存占用和加速推理而对模型权重进行量化，但通常在推理过程中保持激活值为浮点格式 [SSZ21]，以减轻量化带来的信息损失和精度降低。然而，要以密码学方式证明推理的正确性，权重和激活值以及它们之间的所有运算都必须使用整数。因此，一个精确的仅整数推理算法对于高效生成证明是必要的。

> **English:**
> Need for pure-integer inference. Most existing SNARK constructions operate over finite fields. In contrast, LLM computations are defined over real-valued numbers and implemented using floating-point arithmetic. Bridging the gap is non-trivial and can introduce significant inefficiencies and/or potential accuracy loss. This is a well-known issue in SNARK literature and in particular, when proving ML tasks [HCL+ 24, SSPP25, RWB+ 25]. Prior works show that emulating floating-point arithmetic inside a SNARK system remains quite impractical [GJJZ22, EZC+ 25], hence most existing works [QSL+ 25, CTK25, SLZ24, zko24, HCL+ 24, RWB+ 25] adopt fixed-point arithmetic via quantization over a given domain (e.g., 8 bits). However, certain inputs might not be able to be represented correctly as fixed point if they're too small, or on the opposite, too large inputs might blow up the values. Given lookup table range needs to be fixed in advance, some inputs might not be provable at all. Interestingly enough, independently of cryptographic integrity, deep learning practitioners already quantize model weights for reduced memory footprint and faster inference, but typically keep activations in floating-point format during the inference [SSZ21] to mitigate lossiness due to quantization and accuracy degradation. However, to cryptographically prove the correctness of inference, both weights and activations must be in integers as well as all operations between them. Therefore, an accurate integer-only inference algorithm is necessary for efficient proof generation.

**离群激活（Outlier activations）。** LLM 中的离群激活是推理过程中某些层激活张量中出现异常大的值 [RSZ25]。这些极端值通常集中在特定的离群特征通道中，或由层归一化和残差连接等原因引起 [AMC⁺24, LXW⁺24, vBBWN25]，且可能造成严重的量化误差。在执行纯整数推理时这构成更严峻的挑战，因为离群激活可能导致量化分箱过宽，使典型的激活值被舍入到同一个量化值。例如，考虑激活 [9.1, 3.9, 11.5, 1000.0]。如果将此激活量化为 8 位，所有典型激活值都会被量化到 0 或 1，导致完全丧失精度。因此，任何纯整数推理算法都必须解决离群激活问题以保持精度。

> **English:**
> Outlier activations. Outlier activations in LLMs are unusually large magnitude values that occur in the activation tensors of certain layers during inference [RSZ25]. These extreme values, often concentrated in specific outlier feature channels, or caused by reasons such as layer normalization and residual connections [AMC+ 24, LXW+ 24, vBBWN25] and can cause significant quantization errors. This poses an even more significant challenge when performing pure-integer inference as outlier activations can cause the quantization bin to be too wide, leading to typical activations getting rounded to the same quantized value. For instance, consider an activation [9.1, 3.9, 11.5, 1000.0]. If we quantize this activation to 8-bits, all typical activations will get quantized to either 0 or 1, leading to total accuracy loss. Thus, any pure-integer inference algorithm must address the outlier activation problem to preserve accuracy.

**重新量化策略（Requantization strategy）。** 现代对证明者友好的 SNARKs [HLP24, Tea, PP25] 能从直接在更小的有限域上运算（更接近标准 CPU 字长，例如 64 位 Goldilocks 域 [Tea]）中获益。虽然这能加快证明者速度，但在我们可验证 LLM 推理的应用中，它带来了额外挑战。在量化整数推理期间，我们需要在每几次运算（主要是乘法）之后执行重新量化，以防止有限域中的溢出，即将值带回到量化范围内。然而，重新量化本身是一种有损操作，可能进一步降低精度。

> **English:**
> Requantization strategy. Modern prover-efficient SNARKs [HLP24, Tea, PP25] can benefit from being able to operate directly over small finite fields, closer to standard CPU word sizes, (e.g., 64-bit Goldilocks field [Tea]). While this allows for faster prover times, in our verifiable LLM inference application it introduces an additional challenge. During quantized integer inference we need to perform requantization after every few operations (mainly multiplications) to prevent overflow in the finite field, i.e., to bring the values back into the quantization range. However, requantization itself is a lossy operation and can further degrade accuracy.

**操作的批处理证明（Batch proving）。** 证明线性操作（如矩阵乘法）能被 SNARK 系统高效处理。然而，LLM 中所需的非线性操作（如 Softmax、LayerNorm 和 GeLU）对“SNARK 不友好”，需要专门协议以实现最佳效率和精度 [SLZ24, SSPP25]。此外，这些非线性操作在 LLM 推理中被大量且多次使用（与更简单的 ML 任务如神经网络不同）。同时，完整的推理轨迹要求对不同点上的许多多项式进行承诺和打开。最后但同样重要的是，LLM 推理是自回归的，例如需要多次运行前向传播以产生一个向量轨迹。朴素地单独证明每个轨迹将带来难以承受的开销。因此，为提高效率，实用系统必须被设计为批处理证明非线性层的多个实例、多个 PCS 打开以及多个轨迹。

> **English:**
> Batch proving of operations. Proving linear operations such as matrix multiplications are efficiently handled by SNARK systems. However, non-linear operations such as Softmax, LayerNorm, and GeLU that are needed in LLMs, are "SNARK-unfriendly" and require specialized protocols to achieve the best efficiency and accuracy [SLZ24, SSPP25]. Moreover, these non-linear operations are used extensively and multiple times during LLM inference (as opposed to simpler ML tasks such as neural networks). As well, a full trace of inference requires to commit and open many polynomials over different points. Last but not least, LLM inference is auto-regressive, e.g. it runs multiple times a forward pass to produce a vector of trace. Naively proving each trace separately would incur a prohibitive cost. Thus, to increase efficiency, a practical system must be designed to batch the proving of multiple instances of the non-linear layers, the PCS openings, and the multiple traces.

### 1.2 我们的贡献

在本工作中，我们设计并实现了 DeepProve，据我们所知，这是第一个能够在基于 Transformer 的 LLM 上实现高效端到端可验证推理的系统。具体而言，我们完全支持以下模型的可验证推理：（i）GPT-2（1.24 亿参数）[RWC⁺19]，以及（ii）Gemma 3（2.7 亿参数）[Tea25]。

> **English:**
> In this work, we design and implement DeepProve, which, to the best of our knowledge, is the first system that enables efficient end-to-end verifiable LLM inference of transformer-based LLMs. Specifically, we fully support verifiable inference for: (i) GPT-2 (124 million parameters) [RWC+ 19], and (ii) Gemma 3 (270 million parameters) [Tea25]. Our key contributions are as follows:

1. **为多种 LLM 层构建专门的 PIOP 原语**，包括（a）重新量化——量化推理中的一个基本操作，（b）不需要证明者展示平方根和倒数正确计算的归一化层（大幅提升证明者时间并增强精度），（c）多头注意力层，（d）旋转位置编码（参见第 4.3 节）。这与先前只关注 LLM 特定组件（如证明 Softmax [SLZ24] 或高效的矩阵乘 [QSL⁺25]），或只证明单层注意力 [QSL⁺25] 的工作形成鲜明对比。我们形式化证明了每个组件具有逐轮（知识）可靠性 [CCH⁺19]，这使我们在通过 Fiat-Shamir 变换使其非交互化时，能够论证方案的可靠性。

> **English:**
> (1) We build specialized efficient PIOPs for a variety of LLM layers, including (a) re-quantization—a fundamental operation in quantized inference, (b) normalization layers that do not require the prover to show the correct computation of square roots and inverses (dramatically improving prover time and enhancing accuracy), (c) multi-head layer attention, (d) rotary positional embeddings (see Section 4.3). This is in stark contrast with prior works that focus only on specific components of LLMs, such as proving Softmax [SLZ24] or efficient matrix-to-matrix multiplications [QSL+ 25], or only prove single layer attention [QSL+ 25]. We formally prove that each component is round-by-round (knowledge) sound [CCH+ 19], which enables us to argue about the soundness of our scheme when making it non-interactive via the Fiat-Shamir transformation.

2. **将上述协议作为构建模块拼合成完整的 PIOP**，用于证明整个 LLM 模型正确推理。虽然当证明单个 token 的 [SLZ24, QSL⁺25] 正确性时这相对直接，但基于我们之前的讨论，在具有更多 token 的现实场景中如何高效实现尚不清楚。我们提出一种**认证算法（certification algorithm）**，它接收输入和输出 token，并仅通过为最后一个 token 重新执行一次推理步骤来验证后者的正确性。这直接将证明者开销从与 token 数成二次方降到线性（参见第 3.1 节）。然后，我们将所得 PIOP 与多项式承诺方案（PCS）结合，构建 DeepProve——首个端到端可验证的 LLM 推理系统，并应用于 GPT-2 和 Gemma 3 模型。

> **English:**
> (2) Using the above protocols as building blocks, we show how to plug them together in order to construct a PIOP for proving the correct inference of an entire LLM model. While this is relatively straightforward when proving the correctness of a single token [SLZ24, QSL+ 25], based on our prior discussion, it is unclear how to efficiently achieve this in realistic scenarios with more tokens. Instead, we propose a certification algorithm which is given the input and output tokens and validates the correctness of the latter by simply re-executing the inference step only for the final token. This directly reduces the prover overhead from quadratic in the number of tokens to linear (see Section 3.1). We then combine the resulting PIOP with a polynomial commitment scheme (PCS) to build DeepProve, the first end-to-end verifiable LLM inference system, and apply it to the GPT-2 and Gemma 3 models.

3. **提出若干降低证明者开销的实际优化**，例如延迟重新量化、钳制离群激活等，在不牺牲精度的前提下提升效率。

> **English:**
> (3) We propose several practical optimizations to reduce prover overheads, e.g., delayed re-quantization, or clamping outlier activations to improve the efficiency without sacrificing accuracy.

4. **用 Rust 实现并开源了 DeepProve [cod]**，并进行了广泛的性能评估（见第 5 节）。其峰值证明吞吐量在单台机器上对 GPT-2 约 174 token/分钟，对 Gemma 3 约 86 token/分钟，比最先进方案 [QSL⁺25] 快 20–60 倍。验证时间对 GPT-2 约 1.2 秒、对 Gemma 3 约 4 秒；证明大小对 GPT-2 和 Gemma 3 分别为 7–27 MB 和 16–54 MB，为验证者带来合理的部署开销。最后，我们通过在多台机器间横向扩展，设计并评估了 DeepProve 的分布式证明算法（见第 4.5 节）。实验评估表明，分布式 DeepProve 可将证明吞吐量扩展到每 17 台机器 1855 token/分钟。

> **English:**
> (4) We implemented in Rust, open-sourced DeepProve [cod] and extensively evaluated performance (see Section 5). Its peak proving throughput is approximately 174 tokens per minute for GPT-2 and 86 tokens per minute for Gemma 3 (on a single machine), which is 20−60× faster than the state-of-the-art [QSL+ 25]. Verification times are around 1.2 seconds for GPT-2 and 4 seconds for Gemma 3, while proofs are 7-27MB and 16-54MB for GPT-2 and Gemma 3, respectively, yielding reasonable deployment overheads for verifiers. Finally, we design and evaluate a distributed proving algorithm for DeepProve by horizontally scaling across multiple machines (see Section 4.5). In our experimental evaluation, we show that distributed DeepProve can scale the proving throughput to 1855 tokens per minute with 17 machines.

5. **在精度方面**，DeepProve 取得与浮点推理相当的分数（困惑度 Perplexity 和余弦相似度）。例如，我们 12 位量化的整数推理算法对 GPT-2 保持在与浮点基线 0.54% 以内的精度。与先前工作 [SLZ24, QSL⁺25] 不同，我们的量化方法基于标准的 ML 量化技术，无需任何自定义修改。

> **English:**
> (5) In terms of accuracy, DeepProve achieves comparable scores (Perplexity and cosine similarity) with that of floating-point inference. For instance, our quantized integer inference algorithm with 12-bit quantization maintains accuracy within 0.54% of the floating-point baseline for GPT-2. Unlike prior works [SLZ24, QSL+ 25], our quantization approach is based on standard ML quantization techniques without any custom modifications.

6. **作为独立贡献**，我们在 DeepProve 流水线中加入了某种“非幻觉证明（proof-of-non-hallucination）”（见第 4.4 节）。更具体地说，我们提出了一个用于证明最先进的幻觉检测技术 LLM-Check [SBS⁺24] 正确执行的协议，并将其扩展到以最小精度损失在量化推理上工作。因此，DeepProve 不仅让验证者相信 LLM 推理的正确性，还相信生成的结果通过了幻觉检测算法。据我们所知，这是文献中第一个以密码学方式证明 LLM 不产生幻觉的方法。

> **English:**
> (6) As an independent contribution, we include in the DeepProve pipeline a type of "proof-of-non-hallucination" (see Section 4.4). In more detail, we propose a protocol for the proof of the correct execution of a state-of-the-art hallucination detection technique, LLM-Check [SBS+ 24], extended to work on quantized inference with minimal accuracy loss. Thus, DeepProve convinces the verifier, not only about the correctness of the LLM inference but also that the produced result passed the hallucination detection algorithm. To our knowledge, this is the first approach to cryptographically prove LLM non-hallucination in the literature.

总体而言，我们相信我们的工作向着更实用的可验证 LLM 迈出了重要一步，因为它不仅是第一个能对完整推理过程生成证明的完全开源系统，而且我们的证明者比先前工作快 20–60 倍。尽管如此，可验证性仍然给推理过程带来了巨大开销。作为对比，在我们的实验平台上，使用 PyTorch [SSZ21] 在 CPU 上运行的明文 GPT-2 推理，对于 128 个输入 token 和 128 个输出 token（序列长度 256）的提示词，可达到 71.4 token/秒（TPS），而我们的证明者为 3 TPS——大约慢了 24 倍。对于总序列长度相同但输出 token 数小得多的其他查询，明文推理可实现超过 2K TPS，比我们的证明者快近三个数量级。此外，当明文推理利用 GPU 执行或批处理等额外优化时，这一差距在实践中可能进一步扩大。

> **English:**
> Overall, we believe our work takes a significant step towards more practical verifiable LLMs, since it not only is the first fully realized open-sourced system capable of producing proofs for the entire inference process, but our prover is also 20-60× faster than prior works. That said, verifiability still introduces a substantial overhead to the inference process. For comparison, plaintext GPT-2 inference on our experimental testbed, using PyTorch [SSZ21] and running on CPU, achieves 71.4 tokens-per-second (TPS) for prompts with 128 input and 128 output tokens (sequence length 256), compared to 3 TPS for our prover—-roughly 24× slowdown. For other queries with the same total sequence length but much smaller output token size, plaintext inference achieves more than 2K TPS, almost three orders of magnitude faster than our prover. Besides, this gap can grow even further in practice when plaintext inference leverages additional optimizations such as GPU execution or batching strategies.

**文章组织。** 本文的其余部分组织如下。在第 2 节中，我们介绍一些关于 LLM 与量化必要的预备知识；在第 3 节中，我们呈现我们方法的技术亮点。第 4 节包含 DeepProve 的核心设计，包括我们针对不同层的协议及其端到端集成，以及我们的分布式方法。在第 5 节，我们展示评估结果。最后，在第 6 节我们讨论相关工作。

> **English:**
> Organization. The rest of the paper is organized as follows. In Section 2, we present some necessary preliminaries on LLMs and in Section 3, we present a technical highlights of our approach. Section 4 contains the core design of DeepProve, including our protocols for different layers and their end-to-end integration, as well as our approach to distribution. In Section 5, we present our evaluation results. Finally, in Section 6, we discuss related works.

---

## 2 预备知识

这里我们包含一些关于量化和 LLM 的必要背景信息。密码学定义见附录 A。

> **English:**
> We include here some necessary background information for quantization and LLMs. Cryptographic definitions are in Appendix A.

### 2.1 量化

我们使用标准的仿射量化技术将模型权重和激活从浮点数转换为整数表示。把浮点值 x 转换为量化整数值 x̄ 的标准公式 [JKC⁺18] 是：x̄ = s·(x − z)，其中 s 是缩放因子（scale factor），z 是零点（zero-point）。

> **English:**
> We use standard affine quantization techniques to convert the model weights and activations from floating-point to integer representations. The standard formula [JKC+ 18] to convert a floating-point value x into a quantized integer value x̄ is x̄ = s·(x − z), where s is the scale factor and z is the zero-point.

**线性运算与重新量化 [JKC⁺18]。** 考虑矩阵运算 X₄ = X₁X₂ + X₃，其中 X₁ ∈ ℝ^(m×ℓ)，X₂ ∈ ℝ^(ℓ×n)，X₃, X₄ ∈ ℝ^(m×n) 以浮点值表示。将每个矩阵的条目记为 x_α[i,j]（α ∈ {1,2,3,4}）。矩阵中每个条目的量化关系为：x̄_α[i,j] = s_α·x_α[i,j] − z_α。根据带偏置相加的矩阵乘法定义，x̄₄[i,k] = z₄ + M·Σ_{j=1}^ℓ (x̄₁[i,j] − z₁)(x̄₂[i,j] − z₂) + B·(x̄₃[k] − z₃)，其中 M := s₁s₂/s₄，B := s₃/s₄。在实践中，诸如 PyTorch [For25] 这样的 ML 框架设置 s₃ = s₁s₂。由于 M ∈ (0,1)，我们设 M = 2^(−n)·ε（对某个小 ε > 0 和整数 n）。

> **English:**
> Linear operations [JKC+ 18] and requantization. Consider the matrix operation X₄ = X₁X₂ + X₃, where X₁ ∈ ℝᵐˣˡ, X₂ ∈ ℝˡˣⁿ, and X₃, X₄ ∈ ℝᵐˣⁿ represented as floating-point values. We denote entries of each matrix as x_α[i, j] for α ∈ {1, 2, 3, 4}. The quantization relations for each entry in the matrices are given by: x̄_α[i, j] = s_α·x_α[i, j] − z_α. From the definition of matrix multiplication with bias addition: x̄₄[i, k] = z₄ + M·Σ_{j=1}^ℓ (x̄₁[i, j] − z₁)(x̄₂[i, j] − z₂) + B·(x̄₃[k] − z₃), where M := s₁s₂/s₄ and B := s₃/s₄. In practice, ML frameworks, like PyTorch [For25], set s₃ = s₁s₂. Since M is in (0, 1), we set M = 2⁻ⁿ⋅ε for some small ε > 0 and integer n.

### 2.2 大语言模型

虽然 DeepProve 对任何可表示为操作图（MLP、CNN 等）的模型都是通用的，但我们对 LLM 做了特别的效率优化。LLM 主要包括：一个**嵌入层**，将一维输入映射到 LLM 运算的多维空间；多个**顺序的 Transformer 块**，在多维空间中处理输入，每次改变隐藏状态；最后，一个**去嵌入层**，根据隐藏状态选择下一个 token。

> **English:**
> While DeepProve is generic over models representable as a graph of operations (MLPs, CNNs, etc), particular attention have been put into making it efficient on LLMs which mainly consist of: an embedding layer, mapping the user one-dimensional input to the multi-dimensional space the LLM operates on; multiple sequential transformer blocks reasoning about the inputs in the multi-dimensional space, each time changing the hidden state, e.g. the state of the transformed input; at the end, an un-embedding layer that chooses the next token according to the hidden state.

**自回归模型：** LLM 用户首先将整个输入提供给 LLM，生成下一个 token，然后将该 token 追加到输入中，再次运行 LLM。该循环在预定的迭代次数后、或直到 LLM 表示它没有更多可输出的内容时停止。GPT-2 [RWC⁺19] 是首批能够生成可读英文文本的 LLM 模型之一。GPT-2 Transformer 块的详细图示见图 1，Gemma 3 见图 4。

> **English:**
> Auto-regressive models: Users of an LLM first give the whole input to the LLM, generate the next token, and append this token to the input, and run the LLM again. This loop stops after a predetermined number of iterations or until the LLM indicates it has nothing else to produce. GPT-2 [RWC+ 19] is one of the first LLM models that were able to produce readable English. See Fig. 1 for a detailed view of a GPT-2 transformer block, and Fig. 4 for Gemma 3.

（图 1、图 4 为架构示意图，原文见 PDF。）

---

## 3 技术概述

在本节中，我们概述使 DeepProve 在可验证 LLM 推理中同时实现高精度和高性能的关键技术。

> **English:**
> In this section, we present an overview of the key techniques that enable DeepProve to achieve both high accuracy and high performance in verifiable LLM inference.

### 3.1 LLM 推理的认证

首先，我们提出一个**认证算法**来验证在给定输入 token 的情况下，输出 token 由 LLM 模型正确计算。我们的认证算法非常简单：它直接将输入和输出 token 的整个序列一次性喂给模型，本质上是执行一次推理步骤。在实践中，这意味着只证明最后一步推理，使证明者时间仅与输入和输出 token 呈线性，而非朴素推理中的二次方。

> **English:**
> First, we propose a certification algorithm for verifying that the output tokens were correctly computed by the LLM model given the input tokens. Our certification algorithm for this is extremely simple: It just feeds the entire sequence of input and output tokens into the model at once, essentially performing a single inference step. In practice, this translates to proving only the last inference step leading to prover times that are only linear to the input and output tokens rather than quadratic, as in the vanilla inference.

正确性源于注意力机制的工作方式：模型为序列中的每个位置生成 token 预测。如果模型在位置 t 的预测与位置 t+1 声称的 token 匹配，那么位置 t+1 的 token 就被认证为正确计算。由于**注意力掩码**（Attention Mask），模型对第 t 个 token 的计算不能使用第 t+1 个 token 的值。此外，GPT-2（图 1）和 Gemma 3（图 4）最后一层的 **argmax** 为执行提供了确定性。考虑以下示例：

- 提示词（Prompt）："The cat"
- 声称的输出（Claimed output）："sat on the mat"
- 拼接序列（Concatenated sequence）：[The, cat, sat, on, the, mat]
- 预测序列（Predicted sequence）：[cat, sat, on, the, mat, ⊥]

因此，我们的认证算法简单地检查模型在每个位置的预测是否与下一个位置声称的 token 匹配，直到序列结束。

> **English:**
> Correctness stems from the way attention mechanism works, in that the model generates token predictions for every position in the sequence. If the model's prediction at position t matches the claimed token at position t + 1, then the token at position t + 1 is certified as correctly computed. Because of the Attention Mask, the model's calculation for the t-th token cannot use the values of the (t + 1)-th token. Moreover, argmax in the final layer of GPT-2 (Fig. 1) and Gemma 3 (Fig. 4) provides determinism to the execution. Consider the following example:
> • Prompt: "The cat"
> • Claimed output: "sat on the mat"
> • Concatenated sequence: [The, cat, sat, on, the, mat]
> • Predicted sequence: [cat, sat, on, the, mat, ⊥]
> Thus, our certification algorithm simply checks if the model's prediction at each position matches the claimed token at the next position until the end of the sequence.

### 3.2 精确的整数推理

LLM 的线性算子使用量化整数运算实现起来相当直接（如第 2.1 节所述）。然而，Softmax、GeLU、LayerNorm 和 RMSNorm 等非线性操作本质上是具有挑战性的，因为基本的（域上的）代数运算不足以计算这些函数 [Wik25b]。为绕过这一挑战，我们结合使用近似和查找表 [SLZ24]，以使用量化整数算术精确实现这些非线性函数。然而，仅凭这些技术不足以在 LLM 中获得高精度（例如离群激活问题 [AMC⁺24, LXW⁺24, vBBWN25]）。这里我们重点介绍 DeepProve 在整数 LLM 推理中实现高效和高精度的技术。

> **English:**
> The linear operators of LLMs are rather straightforward to implement using quantized integer arithmetic, as described in Section 2.1. However, non-linear operations such as Softmax, GeLU, LayerNorm, and RMSNorm are inherently challenging as basic (field) algebraic operations are not enough to evaluate the functions [Wik25b]. To get around this challenge, we use a combination of approximations and lookup tables [SLZ24] to implement these non-linear functions accurately using quantized integer arithmetic. However, these techniques alone are insufficient to obtain high accuracy in LLMs (e.g., the outlier activations problem [AMC+ 24, LXW+ 24, vBBWN25]). Here we highlight the techniques that we use in DeepProve to achieve efficient and high accuracy in integer LLM inference.

**混合精度推理（Mixed-precision inference）。** DeepProve 可由任意量化位长参数化。我们观察到某些层适合将值保持在高精度。例如，残差层简单地将两个输入相加并返回结果作为输出。然而，注意力块或前馈网络的输入事先被归一化，因此残差和块的输出值范围可能有巨大差异（见图 1 和图 4）。要相加这两个量化张量，它们必须具有相同的缩放因子。在低精度下，将值范围较小的张量重新缩放以匹配值范围较大张量的缩放因子，会带来显著的精度损失。DeepProve 通过为此层使用更大的位长来解决此问题，特别地使量化网格足够精细，以便能同时刻画两个张量的值。

> **English:**
> Mixed-precision inference. DeepProve can be parametrized by any quantization bit-length. We observe that certain layers benefit from keeping values in higher precision. For example, a residual layer simply adds its two inputs and return the result as output. However, the input to an attention block or feed forward network is normalised first so the value range on the residual and the block output can vary dramatically (See Figs. 1 and 4). To add these two quantized tensors, they must have the same scaling factor. In low precision, the process of rescaling the tensor with the smaller value range to have the scaling factor matching the one with larger value range incurs a substantial loss in accuracy. DeepProve deals with this issue by using a larger bit length for this layer, specifically such that the quantization grid is fine enough that values from both tensors can be captured.

**离群值平滑（Outlier smoothing）。** 如前人文献所指出（[AMC⁺24, LXW⁺24, vBBWN25]），LLM 层的输出可能包含大的离群值。在更小位长下生成精确量化的主要挑战之一，是处理这些大离群值的量化。为减小这些离群值的大小，DeepProve 使用平滑技术。为平滑这些值，我们使用正交矩阵。一个正交矩阵 M ∈ ℝ^(n×n) 满足性质 M·Mᵀ = Idₙ 且 det(M) = 1。若写 X ∈ ℝ^(k×n) 为线性层的输入，W ∈ ℝ^(n×l) 为权重矩阵，我们可以观察到 X·W = (X·M)·(Mᵀ·W)，其中 X·M 和 Mᵀ·W 都更容易量化（更多细节见 [CCKDS23]）。为避免在各个地方物化难以量化的 X，乘以 M 的右乘被吸收进前一个线性层。这意味着，若 X := X′·W′（X′ ∈ ℝ^(k×b)，W′ ∈ ℝ^(b×n)），我们就在右侧乘以 M 来修改 W′。这一变换对浮点模型的输出没有影响，但由于线性层输出中观察到的值范围要小得多，因此量化起来容易得多。

> **English:**
> Outlier smoothing. As noted in prior works ([AMC+ 24, LXW+ 24, vBBWN25]), the output of LLM layers can contain large outlier values. One of the main challenges in producing accurate quantization at smaller bit lengths is handling the quantization of these large outlier values. To reduce the size of these outlier values, DeepProve makes use of smoothing techniques discussed in [AMC+ 24, LXW+ 24, vBBWN25]. To smooth these values, we use orthonormal matrices. An orthonormal matrix M ∈ ℝⁿˣⁿ, is a matrix satisfying the properties M · Mᵀ = Idₙ and det(M) = 1. If we write X ∈ ℝᵏˣⁿ for the input to a linear layer and W ∈ ℝⁿˣˡ for the weight matrix we can observe that X · W = (X · M) · (Mᵀ · W), where X · M and Mᵀ · W are both much easier to quantize (see [CCKDS23] for more details). To avoid ever materializing the hard-to-quantize X, the right multiplication by M is absorbed into the previous linear layer. By this, we mean if X := X′ · W′ with X′ ∈ ℝᵏˣᵇ and W′ ∈ ℝᵇˣⁿ then we modify W′ by multiplying on the right by M. This transformation has no effect on the floating-point model's output, but is substantially easier to quantize as the range of values observed as linear layer outputs is far smaller.

**重新量化（Requantization）。** n 个 q 位整数乘积的位长最多为 nq 位，而 n 个 q 位整数相加最多产生一个 q + log n 位的整数。因此，若不干预，LLM 推理中间值的位长会迅速增长，导致计算变慢甚至潜在的域上溢出。为解决此问题，DeepProve 在每几层之后采用重新量化，将中间值的位长减小到可管理的范围。DeepProve 使用训练后静态量化 [SSZ21] 来执行重新量化，这涉及基于中间值在浮点推理中的分布来计算其最优缩放因子和零点。

> **English:**
> Requantization. The bit length of the product of n q-bit integers is at most nq bits, and addition of n q-bit integers produces at most a q + log n bit integer. Thus, without any intervention, the bit-length of the intermediate values in LLM inference would grow rapidly, leading to computation slowdown and even potential field overflow. To address this, DeepProve employs re-quantization after every few layers to reduce the bit-length of intermediate values back to a manageable size. DeepProve uses post-training static quantization [SSZ21] to perform re-quantization, which involves computing the optimal scaling factor and zero-point for intermediate values based on their distribution in floating-point inference.

### 3.3 LLM 推理的高效证明

我们设计专门的协议来高效证明基于 Transformer 的 LLM（如 GPT-2 和 Gemma 3）中每个算子的正确性。回顾第 3.1 节，我们通过对拼接的输入和输出 token 执行推理来认证整个输出序列的正确性。由于这在单次前向传播中生成多个预测，我们可以利用协议的批处理变体来证明多个输出的正确性。

> **English:**
> We design specialized protocols to efficiently prove the correctness of each operator used in transformer-based LLMs, such as GPT-2 and Gemma 3. Recall from Section 3.1 that we certify the correctness of the entire output sequence by performing inference on the concatenated input and output tokens. Since this generates multiple predictions in a single forward pass, we can leverage batched variants of our protocols to prove the correctness of multiple outputs.

我们用于设计算子协议的主要密码学工具是：用于证明线性层的**专项 sum-check 协议** [LFKN90]，以及用于证明非线性层的**基于 LogUp 的查找论证** [PH23]。与著名的 GKR 协议 [GKR08] 类似，我们以逐层方式组合这些算子协议，以获得整个 LLM 推理的端到端证明。注意，这样一来，我们的方法除了模型参数和用于非线性层查找论证的见证（witness）值外，不承诺推理过程中计算出的后续中间值。

> **English:**
> The primary cryptographic tools we use to design our operator protocols are: Specialized sum-check protocols [LFKN90] for proving linear layers, and lookup arguments based on LogUp [PH23] for proving non-linear layers. Similar to the fabled GKR protocol [GKR08], we compose these operator protocols in a layer-by-layer manner to obtain an end-to-end proof of the entire LLM inference. Note that, in this way, our approach does not commit to intermediate values computed during the inference beyond model parameters and the witness values needed for lookup arguments in non-linear layers.

最后，我们在 DeepProve 中部署了若干优化，如**延迟重新量化（delayed-requantization）**。回顾我们必须在每几层之后执行重新量化以缩短中间值的位长。由于重新量化涉及缩放和舍入运算，我们可以将非线性运算与后续的重新量化步骤合并。这使我们能一次证明这两种运算，使用单一的查找论证。在文献 [QSL⁺25] 中，这种方法通常被称为**算子融合（operator fusion）**。我们请读者参阅附录 C 和 B，了解诸如钳制等其他优化，并参阅第 4.4 节了解幻觉检测。

> **English:**
> Finally, we deploy several optimizations in DeepProve, such as delayed-requantization. Recall that we must perform re-quantization after every few layers to shorten the bit-length of intermediate values. Since re-quantization involves scaling and rounding operations, we can merge the non-linear operations with the subsequent re-quantization step. This allows us to prove both operations together using a single lookup argument. In the literature [QSL+ 25], this approach is typically called operator fusion. We refer readers to Appendices C and B for other optimizations such as clamping, and to Section 4.4 for hallucination detection.

---

## 4 DeepProve

在这里，我们描述 DeepProve 的整体系统架构，分为预处理（pre-processing）和证明（proving）两个阶段（图 2）。

> **English:**
> Here, we describe the overall system architecture of DeepProve, split into pre-processing and proving phases (Fig. 2).

【图 2：DeepProve 系统架构（概念示意）】（原文为架构图，此处用文字归纳）：
预处理阶段：模型文件（Safetensors/GGUF/ONNX）→ 解析（DAG+浮点权重）→ 量化（DAG+权重+缩放因子 sf(F_p)）→ 承诺模型权重；证明阶段：输入 → 算子 Op1..OpL → 输出，各算子结合 IOP 与见证（lookup），最终输出声明与证明。

> **English (Fig. 2 caption):** Figure 2: System Architecture of DeepProve. Preprocessing: Safetensors/GGUF/ONNX → Parse → Quantize → Commit (model comm.); Calibration inputs + DAG + W (float) → DAG + W + sf(F_p) → Model comm. Inference: Input → Op1 ... OpL → Output with witness (lookup); Proving: Op1...OpL with IOPs, Input/Output claims, Aggregate claims, Concat proofs.

预处理阶段对任何特定 ML 模型都是一次性步骤（见图 2）。它由三个步骤组成：（1）**解析（Parsing）**，（2）**量化（Quantization）**，以及（3）**对模型权重和算子图的承诺（Commitment）**。下面我们详细描述每一个步骤。

> **English:**
> The pre-processing phase in DeepProve is a one-time step for any specific ML model (see Fig. 2). It consists of three steps: (1) Parsing, (2) Quantization, and (3) Commitment to the model weights and operators graph. Here, we describe each of these steps in detail.

**解析（Parsing）。** 在预处理阶段，我们以 SafeTensors [Fac25b]、GGUF [Fac25a] 或 ONNX [dev19] 格式输入 LLM 模型文件。解析模型以提取浮点精度的算子图和权重。因此，解析结束时我们得到一个算子的有向无环图（DAG），其中每个节点代表一个算子（例如 MatMul、Conv 等），边代表算子之间的张量流。与每个算子相关的权重也被提取并存储到相应的节点中。如果模型已经量化（例如 GGUF），我们会先用提供的缩放因子对权重进行反量化。

> **English:**
> Parsing. In the pre-processing phase, we input the LLM model file in SafeTensors [Fac25b], GGUF [Fac25a] or ONNX [dev19] format. The model is parsed to extract the operator graph and weights in floating-point precision. Thus at the end of parsing, we have a directed acyclic graph of operators where each node represents an operator (e.g., MatMul, Conv, etc.) and edges represent the flow of tensors between operators. The weights associated with each operator are also extracted and stored in the respective nodes. If the model is already quantized, e.g. GGUF, we dequantize the weights first using the provided scaling factors.

**量化（Quantization）。** 预处理的这一步接收权重为浮点表示（例如 FP32）的模型，首先使用仿射量化（见第 2.1 节）将它们转换为 q 位有符号整数表示（例如范围 [−2^(q−1)+1, 2^(q−1)−1]）。

> **English:**
> Quantization. This step of pre-processing takes a model with weights in floating-point representation (e.g., FP32) and first converts them to q-bit signed integer representation (e.g., in range [−2^(q−1) + 1; 2^(q−1) − 1]) using affine quantization (see Section 2.1).

我们的系统无论是权重还是激活，都使用以零为中心的**对称量化**，且与量化位长的选择无关。这样，我们很容易让系统根据需要使用不同的位长（例如 8 位、12 位或 16 位）。具体来说，我们执行训练后静态量化，将模型权重从浮点转为有符号整数表示 [SSZ21]。为量化模型，我们需要为权重和激活计算缩放因子，为此我们使用一个由代表性模型输入组成的**校准数据集（calibration dataset）**。我们使用校准数据集评估模型，收集每层以及每层输入/输出的激活和权重统计信息。利用这些统计信息，我们计算 ML 模型每层激活、权重和输入/输出的缩放因子为 s = 2b/(2a−1)，其中 b 是位宽，x_max 和 x_min 是校准期间观察到的最大和最小激活（相应地为权重）值。由于我们使用对称量化，a = max(|x_max|, |x_min|)。每当推理期间激活超出计算范围时，我们会将其钳制到范围 [−a, a] 内。关于我们为获得最精确缩放因子所采取的统计信息的更多细节见附录 C。

> **English:**
> Our system uses a symmetric quantization centered around zero for both weights and activations, agnostic to the choice of quantization bit length. Thus, we can easily adapt our system to use different bit lengths (e.g., 8-bit, 12-bit, or 16-bit) as needed. Specifically, we perform post-training static quantization to convert the model weights from floating-point to signed integer representation [SSZ21]. To quantize the model we need to compute the scaling factors for the weights and activations, for which we use a calibration dataset consisting of representative inputs to the model. We evaluate the model using the calibration dataset and collect the activation and weight statistics for each layer as well as the input/output of each layer. Using these statistics, we compute the scaling factors for activations, weights and input/output in each layer of the ML model as s = 2b/(2a−1), where b is the bit width, x_max and x_min are the maximum and minimum activation (resp. weights) values observed during calibration. Since we use symmetric quantization, a = max(|x_max|, |x_min|). Whenever the activations are outside the calculated range during inference, we clamp them to be within the range [−a, a]. See Appendix C for more information about the statistics we take to obtain the most accurate scaling factors.

假设一个符号整数值为 −x < 0。为了在有限域 F_p 中表示这个值，我们将其表示为 p − x，其中 p 是域的阶。这样我们将所有有符号整数权重（和激活）转换为其对应的有限域表示。因此，在此步骤结束时，我们拥有以有限域值表示的 ML 模型权重、逐层量化缩放因子，以及解析时提取的算子图。

> **English:**
> Say a signed integer value is −x < 0. To represent this value in a finite field F_p, we compute its equivalent representation as p − x, where p is the order of the field. This way we convert all signed integer weights (and activations) to their corresponding finite field representations. Thus, at the end of this step, we have the ML model weights represented as finite field values, the per-layer quantization scaling factors, and the operator graph extracted during parsing.

**整数到域（Integer to Fields）的映射。** 在模型承诺和证明期间（而非推理期间），DeepProve 将整数权重映射到用于证明的域中。具体来说，整数 i ∈ ℤ 按以下方式映射到域 f ∈ 𝔽：（a）若 i ∈ [0, 2^(q−1) − 1]，则 f 映射为域中的同一数值（假设 |𝔽| 远大于量化位长，例如 64 或 256 位等）；（b）若 i ∈ [−2^(q−1) − 1, 0]，则 f 映射为 ord(𝔽) − 1 − i，即映射到域范围的高端。使用此方法可以证明算术确实成立并产生正确的符号输出。

> **English:**
> Integer to Fields. During model commitment and proving (and not during inference), DeepProve maps the integer weights to the fields chosen for proving. Specifically, an integer i ∈ Z is mapped to a field f ∈ F in the following way. (a) If i ∈ [0; 2^(q−1) − 1] then f is mapped to the same value in the field (|F| is assumed to be much larger than the quantization bit-length, e.g. 64 or 256 etc.). (b) If i ∈ [−2^(q−1) − 1; 0], then f is mapped to ord(F) − 1 − i, i.e., it is mapped to the higher end of the field range. Using this method, it can be shown that the arithmetic actually holds and produces the correct sign output.

**模型承诺（Model Commitment）。** 在此步骤中，我们使用多项式承诺方案（PCS）承诺模型权重。我们的系统和协议对 PCS 的选择无关。设 Wᵢ 表示 ML 模型第 i 层的权重张量，(m, n, ℓ) 为张量的维度。我们将 Wᵢ 表示为一个函数 fᵢ : {0,1}^log m × {0,1}^log n × {0,1}^log ℓ → 𝔽，其中 fᵢ(a,b,c) = Wᵢ[a,b,c]。设模型有 L 层。每层 i 包含一组参数张量 {W_{i,1}, …, W_{i,Cᵢ}}（例如，一个稠密层既有权重矩阵又有偏置向量）。每个张量 W_{i,j} 被表示为一个多线性函数 f_{i,j}。因此整个模型由函数族 {f_{i,j} | 1 ≤ i ≤ L, 1 ≤ j ≤ Cᵢ} 表示。如果 PCS 不支持同态（homomorphism），那么我们使用 PCS 的批量承诺（batch commit）操作计算对所有模型参数的单一承诺。但如果 PCS 支持同态，那么我们分别计算每个多项式的承诺。于是，在此步骤结束时，我们有对模型权重和量化缩放因子的承诺，这些承诺将同时被证明者和验证者使用。

> **English:**
> Model Commitment. In this step, we commit to the model weights using a polynomial commitment scheme (PCS). Our system and protocols are agnostic to the choice of PCS. Let Wᵢ denote the weight tensor for layer i of the ML model and (m, n, ℓ) be the dimensions of the tensor. We represent Wᵢ as a function fᵢ : {0, 1}^log m × {0, 1}^log n × {0, 1}^log ℓ → F, where fᵢ(a, b, c) = Wᵢ[a, b, c]. Let the model have L layers. Each layer i contains a collection of parameter tensors {W_{i,1}, ..., W_{i,Cᵢ}} (for example, a dense layer has both a weight matrix and a bias vector). Each tensor W_{i,j} is represented as a multilinear function f_{i,j}. The full model is therefore represented by the functions {f_{i,j} | 1 ≤ i ≤ L, 1 ≤ j ≤ Cᵢ}. If the PCS doesn't support homomorphism, then we compute a single commitment to all model parameters using the batch commit operation of the PCS. However, if the PCS supports homomorphism, then we compute a commitment to each polynomial separately. Thus, at the end of this step, we have commitments to the model weights and quantization scaling factors that will be used both by the prover and verifier.

### 4.2 证明（Proving）

证明阶段针对 LLM 的每个输入执行。它由三个主要步骤组成：（1）**量化推理**，（2）**见证承诺**，以及（3）**算子证明**。下面我们详细描述每个步骤。

> **English:**
> The proving phase is executed for each input to the LLM. It consists of three main steps: (1) Quantized inference, (2) Witness commitment, and (3) Operator proving. In this subsection, we describe each of these steps in detail.

**量化推理（Quantized Inference）。** 给定 ML 模型的输入，如第 3.2 节所述，我们使用预处理阶段计算出的量化缩放因子执行量化推理。推理按算子逐个执行。对每个算子，我们使用整数算术执行必要的计算。这包括：（1）钳制激活使其保持在量化范围内，（2）平滑离群值，（3）对残差层使用混合精度算术，以及（4）在输出范围过大、难以证明时根据需要重新量化输出。每个算子的输出作为图中后续算子的输入。在此过程中，我们记录**执行轨迹（execution trace）**，其中包括图中每个算子节点产生的所有中间激活和输出。我们还记录证明者所需的任何附加值，例如 Softmax 操作前应用的偏移值（如 [SLZ24]）。该执行轨迹构成证明生成步骤的见证。

> **English:**
> Quantized Inference. Given an input to the ML model from the user, as mentioned in Section 3.2, we perform quantized inference using the quantization scaling factors computed during the pre-processing phase. The inference is executed operator-by-operator. For each operator, we perform the necessary computations using integer arithmetic. This includes: (1) clamping activations to ensure they remain within the quantization range, (2) smoothing out outliers, (3) using mixed-precision arithmetic for residual layers, and (4) re-quantizing the outputs as needed if the output range grows too large and cannot be easily proven. The output of each operator serves as the input to the subsequent operator in the graph. During this process, we record the execution trace, which includes all intermediate activations and outputs produced at each operator node in the graph. We also record any additional values needed for the prover, such as the shift values applied before the Softmax operation, as in [SLZ24]. This execution trace forms the witness for the proof generation step.

**见证承诺（Witness Commitment）。** 在预处理阶段，我们承诺了静态信息（如模型权重和量化缩放因子）。在此步骤中，我们承诺一些非确定性信息。例如，我们使用查找论证来验证非线性计算，具体采用 LogUp GKR 作为查找论证 [PH23]。LogUp GKR 要求将查找表和见证值作为陈述（statement）的一部分进行承诺。因此，在此步骤中，我们承诺这些依赖于模型输入的见证值。我们采用与模型承诺类似的方法，使用批量多项式承诺方案仅承诺那些明确需要被承诺、且无法被虚拟化（virtualized）[Tha25] 的见证值。

> **English:**
> Witness Commitment. During the pre-processing phase, we committed to the static information such as model weights and quantization scaling factors. In this step, we commit to some non-deterministic information. For example, we use lookup arguments to verify non-linear computations, specifically employing Logup GKR as the lookup argument [PH23]. Logup GKR requires committing to the lookup table and the witness values as a part of the statement. Thus, in this step, we commit to these witness values that depend on the input to the model. We employ a similar approach to model commitments and use the batched polynomial commitment scheme to commit only to the witness values that explicitly need to be committed and cannot be virtualized [Tha25].

### 4.3 算子证明（Operator Proving）

假设算子图有 L 层。证明过程逐层进行，类似于 GKR 协议 [GKR08, CMT12]，从 ML 模型的输出层向输入层进行，即按逆序进行。

> **English:**
> Suppose the operator graph has L layers. The proving process is carried out layer-by-layer, similar to the GKR protocol [GKR08, CMT12], proceeding from the output layer to the input layer of the ML model, e.g., in backwards order.

在证明阶段的第一步，证明者就 ML 模型的输出（即算子 L 的输出）做出声明（claim）。该声明实际源自 Fiat-Shamir 变换——在把所有承诺和辅助信息正确整合进 transcript 之后。由于验证者无法直接求值模型，证明者和验证者进行一个 PIOP 协议，证明算子 L 的正确性。在该协议结束时，验证者获得（如图 3 所示）：（1）此算子的正确性证明，（2）关于此算子所用模型参数的声明，（3）关于此层见证（如有）的声明，以及（4）最重要的是关于算子 L 输入的声明。这样，验证者将关于算子 L 输出的声明，转化为关于算子 L−1 输出的声明，即一个虚拟声明 [Tha25]。此过程重复进行，直到证明者和验证者到达模型的输入层。此时验证者可以直接求值输入层算子。利用证明者提供的 PCS 求值证明，验证者检查关于模型参数和各层见证值的声明。如果所有求值声明正确，连同算子证明一起，验证者接受 ML 模型的最终输出为正确。

> **English:**
> In the first step of the proving phase, the prover makes a claim about the output of the ML model, i.e., the output of operator L. This claim is actually derived from the Fiat-Shamir transformation after all commitments and auxiliary information have been correctly integrated into the transcript. Since the verifier cannot evaluate the model directly, the prover and verifier engage in a PIOP protocol that proves the correctness of operator L. At the end of this protocol, the verifier obtains (as illustrated in Fig. 3): (1) a proof of correctness for this operator, (2) claims about the model parameters used in this operator, (3) claims about the witness of this layer if any, and (4) most importantly, claims about the input to operator L. This way, the verifier converts the claim about the output of operator L into a claim about the output of operator L−1, a virtual claim [Tha25]. This process is repeated until the prover and verifier reach the input layer of the model. At this point, the verifier can directly evaluate the input layer operator. Using PCS evaluation proof from the prover, the verifier checks the claims about of the model parameters and witness values at each layer. If all evaluations claims are correct, along with the operator proofs, the verifier accepts the final output of the ML model as correct.

【图 3：DeepProve 中的算子证明（概念示意）】（原文为图，此处文字归纳）。

> **English (Fig. 3):** Operator proving in DeepProve: proof of operator correctness, model/witness claims, and input claims flowing layer-by-layer.

**优化：** 作为优化，验证者可以将模型和见证声明的求值委托给证明者。证明者进而使用 PCS 的批量打开（batching opening）功能，为所有跨层的声明和见证声明生成单个求值证明。

> **English:**
> As an optimization, the verifier can delegate the evaluation of model and witness claims to the prover. The prover in turn uses the batching opening feature of the PCS to produce a single evaluation proof for all claims and witness claims across all layers.

接下来，我们给出 LLM 主要构建模块的证明协议。由于篇幅限制，我们在此描述证明逻辑，并在附录 B 中提供所有详细构造，其中还包括我们所有协议逐轮（知识）可靠性的形式化证明，这使我们能够通过 BCS 变换 [BCS16] 使协议非交互化后，展示其知识可靠性。

> **English:**
> Next, we present proving protocols for the main building blocks of LLMs. Due to space limitations, we describe here the proving logic and provide all detailed constructions in Appendix B, where we also include formal proofs for the round-by-round (knowledge) soundness of all our protocols which enables us to show that our protocol is knowledge sound when making it non-interactive using the BCS transformation [BCS16].

**4.3.1 证明嵌入层（Embeddings）。** 嵌入层用于将 token 映射到一个向量——来自预训练嵌入表的向量——该向量将用作 LLM 推理的输入。更具体地说，设词汇表由 t 个 token 组成，标识符为 V = {0, 1, …, t−1}，且每个 token 由维度 d 的向量表示，称为嵌入维度。给定输入序列 x = (x₀, x₁, …, x_{s−1}) ∈ Tˢ 以及嵌入矩阵 T ∈ F^(v×d)，x 的嵌入定义为 Y = Embed(T, x) = [T[x₀], T[x₁], …, T[x_{s−1}]] ∈ F^(s×d)。

> **English:**
> Proving Embeddings. The embedding layer is used to map a token with a vector—from a pre-trained embedding table—which will be used as input during LLM inference. In more detail, let the vocabulary consist of t tokens with identifiers V = {0, 1, ..., t − 1}, and let each token be represented by a vector of dimension d, called the embedding dimension. Given an input sequence x = (x₀, x₁, ..., x_{s−1}) ∈ Tˢ and an embedding matrix T ∈ Fᵛˣᵈ, the embedding of x is defined as: Y = Embed(T, x) = [T[x₀], T[x₁], ..., T[x_{s−1}]] ∈ Fˢˣᵈ.

为了证明上述，人们可以直接使用任何最先进的查找论证来证明 [PH23]。然而在实践中，这会导致巨大的证明者开销，因为必须额外承诺并打开一个规模为 Ω(sd) 的多项式。而我们观察到，验证者可以直接访问索引向量 x，这意味着验证开销可以与 |x| = s 成正比。因此，我们提出一个轻量级协议，无需额外承诺。我们的关键观察是，我们可以将 Embed 函数重写成一个矩阵乘法，使用 sum-check 协议 [Tha13] 证明起来要高效得多。特别地，Embed(T, x) = Hₓ · T，其中 Hₓ ∈ F^(s×t) 是一个稀疏矩阵，包含 s 个非零元素，使得 Hₓ[i, xᵢ] = 1，其余位置为 0（对所有 i ∈ [s]）。注意，由于 Hₓ 只有 s 个非零元素，证明者无需承诺其多线性扩展（MLE）——验证者可以在 O(s log n) 次域操作内本地求值它。

> **English:**
> In order to prove the above, someone could directly use any state-of-the-art lookup argument to prove the above [PH23]. In practice, however, this would result in large prover overhead since it would have to additionally commit and open to a polynomial of size Ω(sd). Instead, we observe that the verifier has direct access to the index vector x meaning that the verification cost can be proportional to |x| = s. We thus propose a lightweight protocol that requires no additional commitments. Our key observation, is that we can re-write the Embed function as a matrix to matrix product which is much more efficient to prove using the sumcheck protocol [Tha13]. In particular, Embed(T, x) = Hₓ · T, where Hₓ ∈ Fˢˣᵗ is a sparse matrix containing s non-zero elements such that Hₓ[i, xᵢ] = 1 and 0 everywhere else, for all i ∈ [s]. Note that since Hₓ has only s non-zero elements, the prover does not have to commit to its MLE since the verifier can evaluate it locally in O(s log n) field operations.

---

**4.3.2 证明重新量化（Re-quantization）。** 重新量化是量化推理中的一个基本操作，它使推理算法能将 q' 位量化整数 x ∈ ℤ 转换为 q 位量化整数 y ∈ ℤ。正如我们在附录 C 中讨论的，此操作通常在定点乘法之后调用，以将整数"提升"回原始量化水平并避免溢出。在高层次上，重新量化将输入向量 x ∈ Fⁿ（期望为 q' 位）作为输入，并输出 y ← ReQuant(x) ∈ Fⁿ，满足：

```
y[i] ←   2^(q−1),                                    若 x[i] ≥  2^(q'−1)
        −2^(q−1) + 1,                                若 x[i] ≤ −2^(q'−1) + 1
        ⌊(2^f·ε·x[i] + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1),  否则
```

其中 δ 和 f 是在预处理阶段提供的常量。

> **English:**
> Proving Re-quantization. Re-quantization is a fundamental operation in quantized inference, which enables the inference algorithm to turn a q'-bit quantized integer x ∈ Z into a q-bit quantized integer y ∈ Z. As we discuss in Appendix C, this operation is usually invoked after fixed point multiplications to "lift" an integer to the original quantization level and to avoid overflows. At a very high level, re-quantization takes as input a vector x ∈ Fⁿ of values expected to be q'-bit and outputs y ← ReQuant(x) ∈ Fⁿ such that:
> y[i] ← 2^(q−1), if x[i] ≥ 2^(q'−1); −2^(q−1) + 1, if x[i] ≤ −2^(q'−1) + 1; ⌊(2^f·ε·x[i] + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1), otherwise.
> where δ and f are constants provided in a pre-processing phase.

在解释最终结果之前，我们先给出协议的高层思想。我们首先讨论所有 x 的值都是 q' 位数字的简单情况，之后再讨论更一般的情况。

> **English:**
> Before we explain our final result, we first provide a high level idea of our protocol. We will initially discuss how to handle the simple case where all values of x are q'-bit numbers and later we will discuss the more general one.

**情形 1：x 具有正确范围。** 在这种情况下（对应先前工作 [SLZ24, QSL⁺25] 考虑的唯一情形），y = ⌊(2^f·ε·x + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1)。乍看之下，这个公式似乎"对 SNARK 不友好"，因为它需要对每个元素进行一次除法。然而请注意，除数是 2 的幂，因此除法可以视为一次右移，这更容易证明。因此，证明者只需展示一个带常量标量的线性运算之后再进行 f+δ 位的右移操作的正确性，而这可以使用一个基于可分解表查找论证的 PIOP 高效证明 [STW23, AST24]（详细定义见附录 A）。特别地，PIOP 证明者将输入分解为 k 位数，对每个块进行范围检查，最后保留最高的 q/k 个块。在本节其余部分，我们设 k = q。

> **English:**
> Case 1: x has Correct Range. In this case, (which corresponds to the only scenario considered by prior works [SLZ24, QSL+ 25]), y = ⌊(2^f·ε·x + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1). At first glance, this formula seems "SNARK-unfriendly" as it requires one division per element. Observe, however, that the divisor is a power of two number, and thus the division can be considered as a right shift which is easier to prove. Hence, the prover only has to show the correctness of a linear operation with constant scalars followed by a right shift operation of f+δ bits which can be efficiently proven using a PIOP for lookup arguments over decomposable tables [STW23, AST24] (see Appendix A for detailed definition). In particular, the PIOP prover decomposes the input to k-bit numbers, range-checks each chunk and finally keeps the q/k most significant chunks. For the rest of this section, we set k = q.

**情形 2：输入范围不正确。** 在这种情况下，我们需要为原始 ReQuant 函数构造一个 PIOP。为此，我们仍需依赖可分解表的查找论证。首先，假设 |x| 的最大元素是一个 q'' 位数。接下来，证明者将 x′ = Quant(2^f·ε)·x + 2^(f+δ) − 1 分解为 s = q''/q 个块 (x′₁, …, x′_s)，使得 x′ = Σ_{i∈[s]} 2^((i−1)q)·x′ᵢ。然后，它使用这些块作为查找论证的输入，其中表定义如下：前 i ∈ [q'/q] 张表简单地范围检查这些块（例如对所有 j ∈ [2^q]，Tᵢ[j] = j），而所有其他表用于识别超出期望范围的元素。正如我们接下来会更详细阐述的，当一个数超过 q' 位时，这些表输出 1 或 −1，否则输出 0。然后我们使用这些表的输出来计算 y。

> **English:**
> Case 2: Input has Incorrect Range. In this case, we need to construct a PIOP for the original ReQuant function. To achieve this, we will still rely on lookup arguments from decomposable tables. First, assume the largest element of |x| is a q''-bit number. Next, the prover decomposes x′ = Quant(2^f·ε)·x + 2^(f+δ) − 1 in s = q''/q chunks (x′₁, ..., x′_s) such that x′ = Σ_{i∈[s]} 2^((i−1)·q)·x′ᵢ. It then uses these chunks as input into the lookup argument with tables defined as follows: The first i ∈ [q'/q] tables simply range check the chunks (e.g., Tᵢ[j] = j for all j ∈ [2^q]) while all other the tables are used to identify the elements that exceed the expected range. As we will elaborate in more detail next, these tables output 1 or −1 if a number has more than q'-bits and 0 otherwise. We then use the outputs of these tables to compute y.

**协议描述。** 基于以上，我们具备了构造证明重新量化正确性的 PIOP 所需的全部要素。初始时，证明者计算 x′ = Quant(2^f·ε)·x + 2^(f+δ) − 1，双方按照输入 x′ 的查找 PIOP 进行交互。如上所述，在此 PIOP 中，证明者将 x′ 分解为 s 个块 (x′₁, …, x′_s)，并将第 i 个块传入按如下定义的表 Tᵢ：（1）对所有 j ∈ [2^q] 和所有 i ∈ [q'/q]，Tᵢ[j] = j（范围检查）；（2）对所有 j ∈ [2^q]∖{0} 和所有 i ∈ {q'/q, …, s−1}，Tᵢ[j] = 0，且 Tᵢ[0] = 1（检查元素是否属于不正确范围）；（3）若 j ∈ {1, …, 2^(q−1) − 1} 则 T_s[j] = −1；若 j ∈ {2^(q−1) − 1, 2^q − 1} 则 T_s[j] = 1；且 T_s[0] = 0（应用于最高有效位，既范围检查又根据符号输出 −1 或 1）。

> **English:**
> Protocol Description. Based on the above, we have everything we need to construct our PIOP for proving the correctness of requantization. Initially, the prover computes x′ = Quant(2^f·ε)·x + 2^(f+δ) − 1 and both parties interact following the lookup PIOP on input x′. As discussed above, during this PIOP, the prover decomposes x′ in s chunks (x′₁, ..., x′_s) and passes the i-th chunk through the table Tᵢ defined as follows:
> (1) Tᵢ[j] = j, for all j ∈ [2^q] and all i ∈ [q'/q]. These tables range check all chunks that belong to the expected range.
> (2) Tᵢ[j] = 0, for all j ∈ [2^q]\{0} and Tᵢ[0] = 1, for all i ∈ {q'/q, ..., s − 1}. These tables check if an element belongs to an incorrect range by returning 0 if a chunk is non-zero, else 1.
> (3) T_s[j] = −1 if j ∈ {1, ..., 2^(q−1) − 1}, T_s[j] = 1 if j ∈ {2^(q−1) − 1, 2^q − 1} and T_s[0] = 0. Note that this table, applied to the most significant bit, not only checks if an element x′₁[j] is in the correct range but also, if it is, then it outputs −1 or 1 depending on whether x₁[j] is negative or positive.

此外，查找 PIOP 通过证明 x = Σ_{i∈[s]} 2^((i−1)q)·x′ᵢ 来展示分解的正确性，并通过展示下式来证明重新量化输出 y 的正确性：y = InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]}) · (T_{q'/q}[x'_{q'/q}] − 2^(q−1)) + (1 − InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]})) · ((2^(n−1) − 1)/2 + T_s[x_s]/2)，其中 InRange 函数在 x′[i] 是 q' 位数时输出 1，若更大则输出 0。更形式化地，InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]}) = (1 − T_s[x_s]²) · ∏_{i=q'/q+1}^{s−1} Tᵢ[xᵢ]。

> **English:**
> Furthermore, the lookup PIOP shows the correctness of the decomposition by proving that x = Σ_{i∈[s]} 2^((i−1)q)·x′ᵢ and the correctness of the requantized output y by showing that: y = InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]}) · (T_{q'/q}[x'_{q'/q}] − 2^(q−1)) + (1 − InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]})) · ((2^(n−1) − 1)/2 + T_s[x_s]/2), where the InRange function outputs 1 if x′[i] is a q' bit number and 0 if it is larger. More formally, InRange({Tᵢ, xᵢ}_{i∈[q'/q, s]}) = (1 − T_s[x_s]²) · ∏_{i=q'/q+1}^{s−1} Tᵢ[xᵢ].

不难看出，上述函数当且仅当数 x′[j] 的最高 s − q'/q 个块全为零时输出 1。最后，注意到如果 x′[j] 是 q' 位数，则 Comb_y 输出 ⌊(2^f·ε·x[j] + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1)。此外，若 x′[j] 较大，则 Comb_y 根据表 T_s 决定的 x[j] 的符号，输出 −2^(n−1) + 1 或 2^(n−1)。

> **English:**
> It is not hard to see that the above function outputs 1 if and only if the most significant s − q'/q chunks of a number x′[j] are zero. Finally, observe that if x′[j] is a q'-bit number then Comb_y outputs ⌊(2^f·ε·x[j] + 2^(f+δ) − 1) / 2^(f+δ)⌋ − 2^(q−1). Furthermore, if x′[j] is larger then Comb_y outputs −2^(n−1) + 1 or 2^(n−1), depending on the sign of x[j] decided by the table T_s.

**4.3.3 证明 RMSNorm。** RMSNorm 层接收矩阵 X ∈ F^(s×d) 作为输入，并输出 Y ← RMSNorm(X)，其中 Y[i,j] = X[i,j] / sqrt( (1/d)·Σ_{j∈[d]} X[i,j]² )（对所有 i,j ∈ [s]×[d]）。

> **English:**
> Proving RMSNorm. The RMSNorm layer takes as input a matrix X ∈ Fˢˣᵈ and outputs Y ← RMSNorm(X) where: Y[i, j] = X[i, j] / sqrt( (1/d)·Σ_{j∈[d]} X[i, j]² ), ∀i, j ∈ [s] × [d].

证明上述的"标准"方法（所有先前工作 [SLZ24, QSL⁺25] 也都采用）是首先构造一个直接计算 RMSNorm 层的电路，随后调用一个 PIOP 来证明该电路的正确计算。不幸的是，这种方法不仅带来巨大的证明者开销（因为包含许多"对 SNARK 不友好"的运算，如平方根和倒数），而且导致显著精度损失。即，这些技术通常要求将平方根和倒数表示为量化值，这会在每一步引入额外的舍入误差。为克服这些局限，我们提出一种替代方法，它显著更高效（不涉及平方根或倒数），且精度损失可忽略。

> **English:**
> The "standard" approach of proving the above, which was also adopted by all prior works [SLZ24, QSL+ 25], is to first construct a circuit that directly computes the RMSNorm layer and subsequently invoke a PIOP for proving the correct computation of that circuit. Unfortunately, this approach not only incurs significant prover overheads since it contains many "SNARK-unfriendly" operations such as square roots and inverses, but also leads to a significant accuracy loss. Namely, these techniques usually require representing the square root and the inverse as quantized values, which introduces additional rounding error at each step. To overcome these limitations, we propose an alternative approach that is significantly more efficient as it does not involve square roots or inverses and also incurs negligible accuracy loss.

为实现这一目标，证明者计算一个额外的非确定性见证 d ∈ Fˢ，其中 d[i] = Quant(1/sqrt( (1/d)·Σ_{j∈[d]} X[i,j]² ))，并用它通过展示 Y[i,j] = X[i,j]·d[i]（对所有 i,j ∈ [s]×[d]）来证明 Y 的正确计算。不幸的是，这还不够，因为验证者不知道 d 是否被诚实计算。为检查 d 的良构性，证明者向验证者展示 Σ_{j∈[d]} Y[i,j]² ≈ 1。

> **English:**
> To achieve this, the prover computes an additional non-deterministic witness d ∈ Fˢ where d[i] = Quant(1/sqrt( (1/d)·Σ_{j∈[d]} X[i, j]² )) and uses it to prove the correct computation of Y by showing that Y[i, j] = X[i, j] · d[i], for all i, j ∈ [s] × [d]. Unfortunately, this is not enough since the verifier has no clue on whether d has been computed honestly. To check the well-formedness of d, the prover shows to the verifier that Σ_{j∈[d]} Y[i, j]² ≈ 1.

**协议描述。** 基于以上讨论，我们构造我们的 PIOP（协议 10），并呈现一个略微修改的、同时执行输出 Y 的重新量化的协议变体，分为两个基本阶段。**阶段 1：** 展示 Y = ⌊(2^(δ₁)·X + 2^(δ₂·δ₃)) / 2^(δ₂·δ₃)⌋ − 2^(q−1)，其中 δ₁, δ₂, δ₃ 是取决于特定层缩放因子的常量，且 X′[i,j] = X[i,j]·d[i]；证明者计算 d 和 X′，发送 [[f_d]]，随后双方运行 R_requant 的 PIOP 证明 Y 的正确计算。由于验证者不持有 X′ 的 MLE 预言，它收到的 y_{X′} 被还原为 f_X 的求值声明——通过实例 y_{X′} = Σ_{x₁∈{0,1}^log s} f_d(x₁)·Σ_{x₂∈{0,1}^log d} f_X(x₁, x₂) 运行 sum-check。**阶段 2：** 展示 Σ_{j∈[d]} Y[i,j] ≈ Quant(δ₄)；证明者计算向量 y_s ∈ Fˢ（y_s[i] = Σ_{j∈[d]} Y[i,j]），作为输入用于查找 PIOP；此处所列的第二个 sumcheck 实例用于还原为 f_Y 的求值声明，最后由验证者用 [[f_Y]] 验证。

> **English:**
> Protocol Description. Based on the above discussion, we construct our PIOP which we present in Protocol 10, a slightly modified variant which also performs requantization of the output Y, partitioned in two basic phases. Phase 1: the prover shows that Y = ⌊(2^(δ₁)·X + 2^(δ₂·δ₃)) / 2^(δ₂·δ₃)⌋ − 2^(q−1), where δ₁, δ₂, δ₃ are constants that depend on the scaling factors of the specific layer and X′[i, j] = X[i, j] · d[i]. The prover computes d and X′, sends [[f_d]] to the verifier, and both parties run the PIOP for R_requant to prove the correct computation of Y. Note that the verifier does not hold an oracle to the MLE of X′. Instead, it only receives an evaluation claim y_{X′} of f_{X′} which is reduced to an evaluation claim of f_X by running the sumcheck protocol for the instance y_{X′} = Σ_{x₁∈{0,1}^log s} f_d(x₁) · Σ_{x₂∈{0,1}^log d} f_X(x₁, x₂). Phase 2: the prover shows the well-formedness of d by showing that Σ_{j∈[d]} Y[i, j] ≈ Quant(δ₄), where δ₄ a constant selected by the prover in a pre-processing phase. For this, the prover computes the vector y_s ∈ Fˢ such that y_s[i] = Σ_{j∈[d]} Y[i, j] and then uses it as input in the lookup PIOP to show that each element of y_s belongs to the correct range. ... the verifier holds an evaluation claim y_{y_s} of the MLE of y_s and reduces it to an evaluation claim of the MLE of Y by interacting over the sumcheck protocol for the instance y_{y_s} = Σ_{i∈{0,1}^log s} β(i, r_s) · Σ_{j∈{0,1}^log s} f_Y(i, j). Finally, the verifier validates that claim using [[f_Y]]. We have an analysis on the error bound to enforce in Appendix C.

**4.3.4 证明 LayerNorm。** LayerNorm 层接收矩阵 X ∈ F^(s×d) 作为输入，并输出 Y ← LayerNorm(X)，使得 Y[i,j] = (X[i,j] − μᵢ) / sqrt( (1/d)·Σ_{j∈[d]} (X[i,j] − μᵢ)² )（对所有 i,j ∈ [s]×[d]），其中 μᵢ 是 X 第 i 行的均值。

> **English:**
> Proving LayerNorm. The LayerNorm layer takes as input a matrix X ∈ Fˢˣᵈ and outputs: Y ← LayerNorm(X) such that Y[i, j] = (X[i, j] − μᵢ) / sqrt( (1/d)·Σ_{j∈[d]} (X[i, j] − μᵢ)² ), ∀i, j ∈ [s] × [d], where μᵢ, the mean of the i-th row of X.

作为起点，注意对每个 i ∈ [s]，都有：LayerNorm(X[i,:]) = RMSNorm(X[i,:] − μᵢ)。因此，要证明 LayerNorm 的正确计算，只需构造一个 PIOP 来展示：（1）以 X′[i,j] = X[i,j] − d[i] 为输入的 RMSNorm 的正确计算；以及（2）μᵢ = (1/d)·Σ_{j∈[d]} X[i,j]。由于我们已在上一节解释了如何证明（1），这里我们将专注于如何高效证明每个 μᵢ 的正确性。为此，证明者将提供向量 m = (μ₁, …, μ_s) 作为非确定性输入，然后通过展示对每个 i ∈ [s] 有 Quant(1/d)·Σ_{j∈[d]} X[i,j] ≈ μᵢ（通过查找论证）来证明其良构性。

> **English:**
> As a starting point, note that for every i ∈ [s], it holds that: LayerNorm(X[i, :]) = RMSNorm(X[i, :] − μᵢ). Hence, to prove the correct computation of a LayerNorm, it is enough to construct a PIOP for showing (1) the correct computation of RMSNorm with input X′[i, j] = X[i, j] − d[i] and (2) that μᵢ = (1/d)·Σ_{j∈[d]} X[i, j]. Since we explained how to prove (1) in the previous Section, we will focus on showing how to efficiently prove the correctness of each μᵢ. For this, the prover will provide the vector m = (μ₁, ..., μ_s) as a non-deterministic input and then will prove its well-formedness by showing that Quant(1/d)·Σ_{j∈[d]} X[i, j] ≈ μᵢ for all i ∈ [s]—via a lookup argument.

**4.3.5 证明多头注意力层（MHA）。** 在深入我们主要协议的细节之前，回顾多头注意力层（MHA）由多次调用单头注意力层组成，后者以矩阵 Q、K、V ∈ F^(s×d) 的连续列为输入。更具体地说，给定输入矩阵 X ∈ F^(s×d)，我们要证明 Y = [Y₁, …, Y_ℎ] 的正确计算，使得对所有 i ∈ [ℎ]：Yᵢ = Softmax(Qᵢ·Kᵢᵀ/sqrt(d/ℎ))·Vᵢ，其中 Q = [Q₁, …, Q_ℎ] = X·W_Q（K 和 V 类似定义）。

> **English:**
> Proving Multi-Head Attention Layers. Before delving into the details of our main protocol, recall that a multi-head attention layer (MHA) consists of multiple invocations of a single-head attention layer with inputs consecutive columns of the matrices Q, K, V ∈ Fˢˣᵈ. In more detail, given the input matrix X ∈ Fˢˣᵈ, we want to prove the correct computation of Y = [Y₁, ..., Y_ℎ] such that for all i ∈ [ℎ], Yᵢ = Softmax(Qᵢ·Kᵢᵀ/sqrt(d/ℎ))·Vᵢ, where Q = [Q₁, ..., Q_ℎ] = X·W_Q (and likewise we define K and V).

为便于解释，我们首先描述当 d/ℎ 是 2 的幂时证明 MHA 层的协议。之后我们将展示需要做什么修改以克服这一限制。据此，人们可以通过批处理证明 ℎ 个单头注意力层实例（见附录 B.8 的协议 14）来证明 MHA 层的正确计算。这可以按如下方式进行：首先，证明者计算 T_{p,i} = Qᵢ·Kᵢᵀ，T_{s,i} = Softmax(T_{p,i}/sqrt(d/ℎ))，并对所有 i ∈ [ℎ] 发送 [[f_{T_{s,i}}]]。然后，证明者向验证者展示对所有 i ∈ [ℎ]，Yᵢ = T_{p,i}·Vᵢ。注意在此步骤结束时，验证者将获得对每个 Yᵢ 的 MLE 在随机点 r 处的求值声明 y_{Yᵢ}。然而，它无法直接检查其正确性，因为它只有 Y（所有 Yᵢ 的拼接）的 MLE 预言。因此，它必须使用 f_{Yᵢ} 的声明推导出对 f_Y 的求值声明。由于 d/ℎ 是 2 的幂，这可以通过对随机采样的点 r_ℎ ∈ F^log ℎ 计算 y_Y = Σ_{i∈[ℎ]} β(i, r_ℎ)·y_{Yᵢ} 来完成。随后，双方对证明 softmax 的批处理变体 PIOP 进行交互（见附录 B.7 的协议 13），以证明对所有 i ∈ [ℎ]，T_{s,i} = Softmax(T_{p,i}/sqrt(d/ℎ)) 的正确性。在该协议结束时，证明者对每个 f_{T_{p,i}} 获得一个求值声明，这些声明用于下一步中，通过批处理矩阵乘法 PIOP 证明对所有 i ∈ [ℎ]，T_{p,i} = Qᵢ·Kᵢᵀ 的正确性。此时，验证者获得对所有 i ∈ [ℎ] 的 Qᵢ、Kᵢᵀ、Vᵢ 的 MLE 的求值声明。然后它以与 Y 相同的方式，将这些声明重组为对 Q、Kᵀ、V 的 MLE 的单一声明。最后，证明者使用矩阵 Q、Kᵀ、V 的求值声明，通过证明矩阵乘法的 PIOP [Tha13]，来证明 Q = X·W_Q（K、V 类似）。

> **English:**
> To ease explanation, we first describe the protocol for proving an MHA layer when d/ℎ is a power of two. Later we will show what modifications we need to apply in order to overcome that restriction. With that in mind, one can prove the correct computation of an MHA layer by batch proving ℎ instances of a single head attention layer (see Protocol 14 of Appendix B.8). This can be done in the following way: First, the prover computes T_{p,i} = Qᵢ·Kᵢᵀ, T_{s,i} = Softmax(T_{p,i}/sqrt(d/ℎ)) and sends [[f_{T_{s,i}}]] for all i ∈ [ℎ]. Then, the prover shows to the verifier that Yᵢ = T_{p,i}·Vᵢ, ∀i ∈ [ℎ]. Note that at the end of this step, the verifier will end up with evaluation claims of y_{Yᵢ}, for the MLE of each Yᵢ at a random point r. However, it can not directly check their correctness since it only has the oracle of the MLE of Y—the concatenation of all Yᵢ. Hence, it has to derive an evaluation claim of f_Y using the claims of f_{Yᵢ}. Since d/ℎ is a power of two, this can be done by computing y_Y = Σ_{i∈[ℎ]} β(i, r_ℎ)·y_{Yᵢ}, for a randomly sampled point r_ℎ ∈ F^log ℎ. Subsequently, both parties interact over the batched variant of the PIOP for proving softmax (see Protocol 13 Appendix B.7 for more details) to prove the correctness of T_{s,i} = Softmax(T_{p,i}/sqrt(d/ℎ)). At the end of this protocol, the prover ends up with an evaluation claim for each f_{T_{p,i}} which are used in step to prove the correctness of T_{p,i} = Qᵢ·Kᵢᵀ for all i ∈ [ℎ] using the PIOP for batched matrix multiplication. At this point, the verifier ends up with evaluation claims of the MLEs of Qᵢ, Kᵢᵀ and Vᵢ for all i ∈ [ℎ]. It then recombines these claims into one for the MLE of Q, Kᵀ and V in the same way as it did for Y. Finally, the prover uses the evaluation claims of the matrices Q, Kᵀ and V to prove that Q = X·W_Q (and likewise for K, V) using the PIOP for proving matrix multiplications [Tha13].

**处理 d/ℎ 不是 2 的幂的情况。** 在上述协议中，我们假设 d_k = d/ℎ 是 2 的幂，这使得验证者能从相应子矩阵高效计算 Y、Q、Kᵀ、V 的 MLE 的求值声明。不幸的是，当 d_k = d/ℎ 不是 2 的幂时（这通常是实际情况），该算法不成立。为解决此问题，证明者首先需要将矩阵 Y, Q, K, V ∈ F^(s×d) 填充为 Ŷ, Q̂, K̂, V̂ ∈ F^(s×d̃_k·ℎ)，其中 d̃_k 是 d_k 的下一个 2 的幂，使得 Yᵢ, Q̂ᵢ, K̂ᵢ, V̂ᵢ ∈ F^(s×d̃_k)，且最后 d̃_k − d_k 列全为零。此外，它必须展示这些矩阵已被正确填充。这是通过使用矩阵乘法 PIOP 展示（i）Ŷ = Y·P₁（所有其他矩阵也类似），其中 P₁ ∈ {0,1}^(d×d̃_k·ℎ) 是一个填充矩阵，恰好包含 d 个非零元素并适当重排 Q 的列。注意由于 P₁ 只有 d 个非零元素，验证者只需 O(d) 次域操作即可求值其 MLE。

> **English:**
> Handling the Case When d/ℎ is not a Power of Two. In the above protocol, we assumed that d_k = d/ℎ was a power of two, which allowed the verifier to efficiently compute the evaluation claims of the MLEs of Y, Q, Kᵀ and V from the corresponding sub-matrices. Unfortunately, this algorithm does not work when d_k = d/ℎ is not power of two—which is usually the case. To overcome this issue, the prover first needs to pad the matrices Y, Q, K, V ∈ Fˢˣᵈ to Ŷ, Q̂, K̂, V̂ ∈ Fˢˣ(d̃_k·ℎ), where d̃_k the next power of two of d_k, such that Yᵢ, Q̂ᵢ, K̂ᵢ, V̂ᵢ ∈ Fˢˣd̃_k where the last d̃_k − d_k columns are zero. In addition, it has to show that these matrices have been correctly padded. This is done by using the PIOP for matrix multiplication showing that (i) Ŷ = Y·P₁ (and likewise for all other matrices), where P₁ ∈ {0,1}^(d×d̃_k·ℎ) is a padding matrix consisting of exactly d non-zero elements and properly re-arranges the columns of Q. Note that since P₁ has only d non-zero elements, the verifier can evaluate its MLE using only O(d) field operations.

---

**4.3.6 处理非确定性（Non-Determinism）。** 到目前为止，我们都在假设推理是确定性的前提下描述我们的证明者。然而，与 CNN 或 MLP 等许多标准 ML 模型不同，LLM 推理可以是随机的。例如，在解码期间，模型可能从 top-k 个最可能候选 token 中采样下一个 token，而不是确定性地选择最高概率 token。在实践中，这并非局限，因为任何随机化算法都可以通过提供一个单一的纯随机种子作为附加输入而被"去随机化"。因此，为处理非确定性推理，DeepProve 简单地使用我们上面描述的 PIOP 和 [KPPS20, PP24] 提出的验证去随机化部分技术，为我们的认证算法生成一个证明（只要初始随机种子正确，其正确性就有保证）。通常，这些部分对应最终解码阶段，这会带来可忽略的证明者开销。实际上，主要挑战在于真正展示初始随机种子被正确采样。这可以通过使用诸如区块链之类的公开可验证随机源，或通过与验证者的交互式协议（[PGL⁺26] 第 4.3 节）来确保。我们注意到，与所有先前可验证 LLM 推理工作 [SLZ24, QSL⁺25] 一样，我们未在我们的实现中包含这一部分。

> **English:**
> Handling Non-Determinism. So far, we have described our prover under the assumption that inference is deterministic. However, unlike many standard ML models such as CNNs or MLPs, LLM inference can be randomized. For example, during decoding, the model may sample the next token from the top-k most probable candidates instead of deterministically selecting the highest-probability token. In practice, this is not a limitation since any randomized algorithm can be "de-randomized" by providing a single purely random seed as additional input. Consequently, to handle non-deterministic inference, DeepProve simply generates a proof for our certification algorithm (the correctness of which is guaranteed as long as it takes as the initial random seed) using the PIOPs we described above together with the techniques proposed by [KPPS20, PP24] for verifying the de-randomized parts of the inference. Typically, those parts correspond to the final decoding stage, which incurs a negligible prover overhead. In reality, the main challenge comes from actually showing that the initial random seed has been sampled correctly. This can be ensured by either using a public verifiable source of randomness like a blockchain or through an interactive protocol with the verifier (Section 4.3 of [PGL+ 26]). We note that we do not include this in our implementation, same as all prior works on verifiable LLM inference [SLZ24, QSL+ 25].

### 4.4 LLM 非幻觉的证明（Proofs of LLM Non-Hallucination）

尽管 LLM 具有惊人的生成能力，但它们经常生成**幻觉（hallucinated）内容** [JLF⁺23, ZPM⁺24]，即句法上看似合理但语义上错误或缺乏证据支持的内容。这种幻觉破坏了用户的信任，并限制了其在科学、医疗和法律场景中的应用。

> **English:**
> Despite LLM's impressive generative capabilities, they often generate hallucinated content [JLF+ 23, ZPM+ 24], i.e., outputs that are syntactically plausible yet semantically false or unsupported by evidence. Such hallucinations undermine the trust of users and limit their applicability in scientific, medical, and legal settings.

为解决幻觉问题，我们在 DeepProve 中开发了一个模块，与推理证明一起，以零知识的方式证明 LLM 模型生成的响应没有幻觉。关于 LLM 的幻觉检测算法已有大量文献 [MLG23, SBS⁺24, AKKP25, NWZ⁺24, OTG⁺25, GCX⁺25]，参见 [ea25b] 以获得该主题的综述。这些算法可根据其所需设置和信息分类为几类。例如，有仅以输出或生成 token 的 logits 对 LLM 进行黑盒访问的检测算法 [MLG23, FKKG24]，也有需要白盒访问 LLM 内部状态的算法 [CLC⁺24, SBS⁺24]。有利用基于检索增强生成（RAG）的外部参考数据库的算法 [MAB⁺24, LYZ24]，也有可能通过另一个 LLM 进行后处理的算法 [FKKG24]。

> **English:**
> To address the issue of hallucinations, we develop a module in DeepProve that proves the response generated by the LLM model is not hallucinated in zero-knowledge, together with the proof of inference. There is a rich literature on hallucination detection algorithms for LLM [MLG23, SBS+ 24, AKKP25, NWZ+ 24, OTG+ 25, GCX+ 25], see [ea25b] for a survey of the topic. These algorithms can be classified into several categories depending on the settings and information needed. For example, there are detection algorithms with black-box access to the LLM with only the output or the logits of the generated tokens [MLG23, FKKG24], while others need white-box access with internal states of LLM [CLC+ 24, SBS+ 24]. There are algorithms utilizing external reference databases via retrieval-augmented generation (RAG) [MAB+ 24, LYZ24], and there are algorithms with post-processing possibly by another LLM [FKKG24].

我们首先识别适合证明的幻觉检测算法类别。在我们的设置中，由于证明者知晓 LLM 的所有信息并已经计算了内部状态，我们利用具有白盒访问权限的算法来提高检测精度。此外，由于我们旨在与推理证明一起生成非幻觉证明，那些需要外部数据库或后处理的算法并不可取。因此，我们选择 [SBS⁺24] 中最先进且无需外部知识的白盒幻觉检测算法，命名为 **LLM-Check**。随着幻觉检测领域不断发展，我们的技术可以轻松推广到该类别的其他检测算法。

> **English:**
> We first identify the appropriate category of hallucination detection algorithms that is friendly to prove. In our setting, as the prover knows all the information of the LLM and already computes the internal states, we utilize the algorithms with white-box access to the LLM to improve the accuracy of detection. Moreover, as we aim to generate the proof of non-hallucination together with the proof of inference, those algorithms with external databases or post-processing are not desirable. Therefore, we choose the state-of-the-art white-box hallucination detection algorithm without external knowledge in [SBS+ 24] named LLM-check in our protocol. Our technique can be readily generalized to other detection algorithms in this category as the field of hallucination detection continues to evolve.

在 LLM-Check [SBS⁺24] 中，检测基于**注意力得分（Attention Score）**，其中注意力层中 Q 和 K 的核相似度定义为 Ker(Q,K) = softmax(Q·Kᵀ/√d)。注意这正是与附录 B.8 中证明注意力层时计算的矩阵 T_s 相同。注意力得分随后定义为 log det(Ker(Q,K)) = Σ_{i=0}^{s−1} log Ker_{i,i}，即矩阵 Ker(Q,K) 行列式的对数。由于矩阵在掩码后为下三角，其行列式是对角线元素（记为 Ker(i,i)）的乘积。

> **English:**
> In LLM-check [SBS+ 24], detection is based on the Attention Score, with kernel similarity of Q and K in the attention layer defined as Ker(Q, K) = softmax(Q·Kᵀ/√d). Note that it is the same as the matrix T_s computed in Appendix B.8 to prove the attention layer. The attention score is then defined as log det(Ker(Q, K)) = Σ_{i=0}^{s−1} log Ker_{i,i}, the logarithm of the determinant of matrix Ker(Q, K). As the matrix is lower triangular after masking, its determinant is the product of the elements on the diagonal, denoted as Ker(i, i).

对于多头注意力，注意力得分通过取所有头的核相似度的平均值进行聚合。随后将注意力得分与一个阈值进行比较，以判断响应是否被幻觉。注意力得分的计算可以作为一个子协议在 DeepProve 中轻松证明。如上所述，矩阵 Ker 已在推理过程中被计算。我们使用查找论证来近似对数函数，随后使用 sum-check 协议将注意力得分计算为求和。该协议见附录 B.9。（注：该论文还提出了隐藏得分，但实验表明其性能不如注意力得分。）

> **English:**
> For multi-head attention, the attention score is aggregated from the kernel similarity of all heads by taking the average. The attention score is then compared with a threshold to determine whether the response is hallucinated or not. The computation of the attention score can be easily proven as a sub-protocol in DeepProve. As mentioned above, the matrix Ker has already been computed during the inference. We use a lookup argument to approximate the logarithm function, followed by a sumcheck protocol to compute the attention score as a summation. The protocol is presented in Appendix B.9. (Footnote: The paper also proposes the hidden score, but its performance is worse than the attention score in the experiments.)

### 4.5 分布式 DeepProve 证明者（Distributed DeepProve Prover）

在这里，我们讨论如何将 DeepProve 的证明过程分布到多台机器上，共同生成推理过程的证明。这不仅降低了单台机器的工作负载，还减少了相应的内存占用，从而迈向更可扩展的解决方案。

> **English:**
> Here, we discuss how the DeepProve proving process can be distributed across multiple machines that jointly produce the proof of the inference process. This not only reduces the per-machine workload but also the corresponding memory footprint, moving towards more scalable solutions.

简而言之，我们选择通过将表示 LLM 的 DAG 划分为连续层的子集（称为**块 chunk**）进行水平分布式证明负载。每个块可以被视为一个独立的 DAG，可以像单证明者情形一样被证明。DeepProve 分布式机制的关键洞见是，证明的三个步骤（见证承诺、证明声明、承诺打开）可以在图的一个子集上以很小的额外代价执行，且无需依赖底层密码学原语的特定特性。通过承诺每个块的输入/输出，每个块的证明变得独立于其他所有块，这意味着所有块可以同时被证明。

> **English:**
> In a nutshell, we choose to distribute the proving workload horizontally by partitioning the DAG representing the LLM into subsets of consecutive layers, hereby called chunks. Each chunk can be considered as a standalone DAG, which can be proven as in the single-prover case. The key insight for DeepProve's distribution mechanism is that the three steps of proving (witness commitment, proving claims, and commitment opening) can be performed on a subset of the graph with little added cost, and without relying on specific features of the underlying cryptographic primitives. By committing to inputs/outputs of each chunk, proving of each chunk becomes independent from all the other chunks, which means that all the chunks can be proven simultaneously.

在实践中，持有推理轨迹的服务器（协调者 coordinator）将其拆分为子轨迹——每个子轨迹对应一个特定分区——并将它们分发给工作者（workers）。收到子轨迹后，每个工作者运行 DeepProve 的证明算法，就像该分区本身是完整的图一样。注意，此步骤为对应于子图的见证以及用于生成子轨迹的模型多项式生成打开证明。最后，最终证明就是所有子证明的拼接——每个工作者一旦完成就将其子证明发送给协调者。

> **English:**
> In practice, the server holding the inference trace (coordinator) splits it into subtraces—where each subtrace corresponds to a specific partition—and distributes them to workers. Upon receiving the subtrace, each worker runs the proving algorithm of DeepProve as if the partition is a full graph in itself. Note that this step produces opening proofs both for the witness corresponding to the subgraph and the model polynomials used to produce the subtrace. Finally, the final proof is simply the concatenation of all sub-proofs–every worker sends it sub-proof to the coordinator as soon as it is done.

注意，每个工作者承诺每个块的输入和输出。为正确地将所有块绑定在一起，验证者必须检查每个块的每个输入都对应于下一个块的某个输出（输出同理）。为强制这一性质，验证者需要检查：（i）证明者选择的图分区与原始模型 DAG 一致（例如，证明者没有在块中省略节点或边）；（ii）对某块输入的每个承诺都对应于下一个块相应输出的承诺（响应于输出）。我们的水平分区策略提供两个非常有用的优势。首先，注意一旦某块中所有层的推理轨迹就绪，协调者可以立即将该部分轨迹发送给相应的工作者开始证明。这种异步通信可减少将轨迹发送给工作者所需的延迟（除模型最后一个块外）。其次，与许多现有的分布式证明方案（例如 [XZC⁺22, LXZ⁺24]）不同，我们的方法在工作者和协调者之间只需一轮交互，这可以再次显著降低延迟，尤其是在 WAN 环境下。

> **English:**
> Note that each worker commits to the inputs and outputs of each chunk. To correctly bind together all the chunks, it is necessary for the verifier to check that each input of each chunk corresponds to an output of the next chunk (and similarly for outputs). To enforce this property, the verifier needs to check that (i) The partitioning of the graph chosen by the prover is consistent with the original model DAG (e.g., the prover did not omit nodes or edges in the chunks), and (ii) each commitment to an input of a chunk corresponds to the commitment of the corresponding output of the next chunk (resp. for outputs). Our horizontal partitioning strategy offers two very useful advantages. First, observe that once the inference trace for all the layers in a chunk is ready, the coordinator can immediately send this portion of the trace to the corresponding worker to start proving. This asynchronous communication can reduce the latency required to send the trace to the workers (except for the last chunk of the model). Second, unlike many existing distributed prover schemes (e.g., [XZC+ 22, LXZ+ 24]), our approach requires a single round of interaction between workers and coordinator, which can again significantly reduce latency, especially over WAN.

---

## 5 评估（Evaluation）

在本节中，我们评估 DeepProve 的性能和精度。

> **English:**
> In this section, we evaluate DeepProve's performance and accuracy.

**基准设置（Benchmarking setup）。** 我们的主基准机器为 AMD EPYC 9254（24 核，2.9 GHz，504 GB RAM）。然而，对于较小的模型和序列长度，我们也在次机器上基准测试：AMD Ryzen 9 7950X3D（16 核，4.2 GHz，128 GB RAM）。注意，我们的主机器拥有显著更多的内存和 CPU 核，这使我们能基准测试更大的模型和更长的序列长度。另一方面，我们次机器的 CPU 更快，导致更高的吞吐量（表 2）。除非另有说明，所有实验都在主机器上运行。

> **English:**
> Benchmarking setup. Our primary benchmarking machine has AMD EPYC 9254 with 24 cores (2.9 GHz) and 504 GB RAM. However, for smaller models and sequence lengths, we also benchmark on a secondary machine with AMD Ryzen 9 7950X3D with 16 cores (4.2 GHz) and 128 GB RAM. Observe that our primary machine has significantly more memory and CPU cores, which allows us to benchmark larger models and longer sequence lengths. On the other hand, the CPU on our secondary machine is faster, leading to higher throughput (Table 2). Unless otherwise specified, all experiments are run on the primary machine.

我们使用 Scroll Technologies 的 Ceno 库 [Scr24]（基于 Goldilocks 域）用 Rust 实现了 DeepProve 的非交互变体。我们使用 BaseFold [ZCF24] PCS（BF）实例化 DeepProve。此外，我们扩展了该库以支持 BN254 椭圆曲线和 HyperKZG [ZSCZ25] PCS（HKZG），因此我们共有两个具有不同权衡的 DeepProve 版本（例如，BaseFold 版本是合理的后量子安全，而 HyperKZG 版本实现更短的证明）。我们在以下仓库中提供了 DeepProve 的实现 [cod]。

> **English:**
> We implemented the non-interactive variant of DeepProve in Rust using Scroll Technologies' Ceno library [Scr24], which is based on the Goldilocks field. We instantiate DeepProve using the BaseFold [ZCF24] PCS (BF). Furthermore, we extend the library to also support BN254 elliptic curve and HyperKZG [ZSCZ25] PCS (HKZG), so we overall have two versions of DeepProve with different trade-offs (e.g., the Basefold version is plausible post-quantum secure, while the HyperKZG version achieves shorter proofs). We provide an implementation of DeepProve in the following repository [cod].

**实验（Experiments）。** 针对各种最大序列长度，我们使用 12 位量化级别，在 GPT-2 [RWC⁺19] 和 Gemma 3 [Tea25] 模型（分别有 1.24 亿和 2.7 亿参数）上运行推理。我们测量以下参数：（1）生成密码学上下文（包括计算 PCS 参数和承诺模型权重）所需的时间；（2）证明端到端推理所用时间（排除推理本身的时间，因为各种 ML 库已针对推理优化）；（3）验证时间；（4）证明大小；（5）空间利用率；以及（6）token/分钟（TPM）的吞吐量。特别是最后一个指标对比较不同系统非常有用，因为该领域大多数工作并未完全开源。这些实验结果见主机器和次机器的表 1 和表 2。最后，在表 7 中，我们还通过给出逐层的证明者时间分解，对证明者成本进行了更详细的分析。

> **English:**
> Experiments. For various maximum sequence lengths, we run inference on the GPT-2 [RWC+ 19] and Gemma 3 [Tea25] models (with 124M and 270M parameters respectively) using DeepProve using 12-bit quantization level. We measure the following parameters: (1) time taken to generate the cryptographic context, which includes computing the PCS parameters and committing to the model weights; (2) time taken to prove the end-to-end inference, excluding the inference time itself since various ML libraries are already optimized for inference; (3) verification time; (4) proof size; (5) space-utilization and (6) throughput in tokens per minute (TPM). Especially, this last metric is very useful for comparison of different systems, as most works in the area are not fully open-sourced. The results of these experiments can be found in Table 1 and Table 2 for primary and secondary machine, respectively. Finally, in Table 7 we also include a more detailed analysis of the prover cost by presenting a layer-wise breakdown of the prover time.

【表 1：DeepProve 在主机器上的性能（TPM 表示 token/分钟）】
> **Table 1: Performance of DeepProve on our primary machine. TPM denotes the throughput in tokens per minute.**

| 模型 | PCS | 序列长度 | 上下文(min) | RAM(GiB) | 证明(min) | 验证(s) | π(MiB) | TPM |
|------|-----|---------|------------|---------|----------|--------|--------|-----|
| GPT-2 | BaseFold | 64 | 18.47 <1.38 | 0.8 | 1.72 | 20.68 | 80.5 |
| | | 128 | 20.95 | 1.22 | 1.85 | 21.83 | 104.63 |
| | | 256 | 27 | 2.18 | 1.97 | 23.04 | 117.59 |
| | | 512 | 41.98 | 4.44 | 2.12 | 24.33 | 115.26 |
| | HKZG | 64 | 24.41 | 1.09 | 1.81 | 8.98 | 58.68 |
| | | 128 | 30.63 <0.86 | 1.44 | 1.99 | 9.89 | 89.03 |
| | | 256 | 45.01 | 2.23 | 2.20 | 10.83 | 114.87 |
| | | 512 | 78.7 | 4.02 | 3.22 | 11.8 | 127.23 |
| Gemma 3 | BaseFold | 64 | 45.52 | 1.85 | 2.45 | 36.31 | 34.62 |
| | | 128 | 51.66 <3.86 | 2.82 | 2.52 | 38.59 | 45.37 |
| | | 256 | 63.55 | 4.77 | 2.66 | 41.07 | 53.62 |
| | | 512 | 96.27 | 8.56 | 2.88 | 43.6 | 59.8 |
| | HKZG | 64 | 57.65 | 2.62 | 2.59 | 12.62 | 24.45 |
| | | 128 | 69.92 <3.02 | 3.33 | 2.78 | 13.98 | 38.46 |
| | | 256 | 94.52 | 4.77 | 2.98 | 15.25 | 53.67 |
| | | 512 | 147 | 7.93 | 3.43 | 16.66 | 64.53 |

【表 2：DeepProve 在次机器上的性能（TPM 表示 token/分钟，OOM 表示内存耗尽）】
> **Table 2: Performance of DeepProve on our secondary machine. TPM and OOM denote the throughput in tokens per minute and memory exhaustion respectively.**

| 模型 | PCS | 序列长度 | 上下文(min) | RAM(GiB) | 证明(min) | 验证(s) | π(MiB) | TPM |
|------|-----|---------|------------|---------|----------|--------|--------|-----|
| GPT-2 | BaseFold | 64 | 18.33 | 0.57 | 1.35 | 20.66 | 112.86 |
| | | 128 | 20.82 <0.91 | 0.85 | 1.45 | 21.82 | 150.98 |
| | | 256 | 27.02 | 1.46 | 1.55 | 23.02 | 175.08 |
| | | 512 | 41.85 | 2.94 | 1.65 | 24.33 | 174.32 |
| | HKZG | 64 | 24.34 | 0.91 | 1.45 | 8.98 | 70.55 |
| | | 128 | 30.28 <0.79 | 1.20 | 1.59 | 9.89 | 106.96 |
| | | 256 | 44.73 | 1.88 | 1.74 | 10.83 | 135.9 |
| | | 512 | 78.49 | 3.51 | 2.59 | 11.8 | 146.03 |
| Gemma 3 | BaseFold | 64 | 45.38 | 1.36 | 1.91 | 36.21 | 46.95 |
| | | 128 | 51.77 <2.64 | 1.99 | 2.02 | 38.6 | 64.38 |
| | | 256 | 62.96 | 3.26 | 2.08 | 41.04 | 78.59 |
| | | 512 | 96.02 | 5.9 | 2.13 | 43.6 | 86.76 |
| | HKZG | 64 | 57.59 | 2.08 | 2.04 | 12.62 | 30.73 |
| | | 128 | 69.54 <2.47 | 2.71 | 2.19 | 13.91 | 47.17 |
| | | 256 | 96.59 | 4.03 | 2.34 | 15.25 | 63.45 |
| | | 512 | — | — | — | — | OOM |

**证明者时间与上下文生成时间。** 总体而言，我们的证明者时间随模型大小线性扩展。同时，我们的证明者时间随更大序列长度而增加——正如预期——但这种增加并非严格线性。这源于某些运算（如矩阵乘法）的次线性证明者成本。正如我们接下来将讨论的，这导致当序列长度增长时 TPM 更快，从而每个 token 的摊还证明者成本更低。

> **English:**
> Prover time and context generation time. Overall, our prover time scales linearly with the model size. At the same time, our prover time increases with larger sequence length—as expected—but this increase is not strictly linear. This stems from the sub-linear prover cost of certain operations like matrix-to-matrix multiplications. As we will discuss next, this results in faster TPMs when sequence length grows leading to a lower amortized prover cost per token.

我们观察到，随着序列长度增加，HyperKZG（HKZG）比 BaseFold 具有更好的扩展性质。这是因为 HKZG 更高效地计算对见证多项式的承诺，而在更高文本长度下这在整个证明时间中占比更大；我们对 HKZG 的实现也针对小位长标量进行了优化，这正好是 LLM 轨迹所包含的内容。此外，由于 HKZG 具有同态性质，DeepProve 实际上先随机组合所有要打开的多项式为一个整体再进行打开，而 BaseFold 在打开时仍处理独立多项式。无论 PCS 如何，Gemma 3 由于模型更大，其证明者时间都比 GPT-2 高。另一方面，由于 PCS 相关操作在此阶段占主导，上下文生成时间对于可变序列长度相对稳定。

> **English:**
> We observe that HyperKZG (HKZG) has better scaling properties than BaseFold as the sequence length increases. This is due to HKZG being more efficient to compute commitments to witness polynomials, which become more relevant on the overall proving time with higher context lengths; our implementation of HKZG is also optimized for small bitlength scalars, which is exactly what the trace contains in the case of LLMs. Furthermore, since HKZG has homomorphic properties, DeepProve actually randomly combine all polynomials to be opened in a single one first before opening, while Basefold still deals with independent polynomials during opening. Regardless of the PCS, Gemma 3 has a higher prover time than GPT-2 due to its larger model size. On the other hand, context generation time is relatively stable for variable sequence lengths since PCS-related operations dominate this phase.

次机器上的 CPU 具有更高时钟频率，尽管核数和内存较少，仍导致更快的计算。在可比较的序列长度下，次机器的吞吐量比主机器高约 1.5 倍。值得注意的是，我们在实验中实现的最佳 TPM 是 175，这是在次机器上对序列长度为 256 的 GPT-2 得到的。

> **English:**
> CPU on the secondary machine has a higher clock speed, leading to faster computation despite having fewer cores and less memory. At comparable sequence lengths, the throughput on the secondary machine is around 1.5× higher than the primary machine. Remarkably, the best TPM we achieved in our experiments is 175, obtained for GPT-2 at sequence length 256 using the secondary machine.

**证明者成本的分解。** 表 7 报告了使用 BaseFold 对序列长度为 512 的 GPT-2 和 Gemma 3 推理进行 DeepProve 的详细证明者成本分解。具体来说，它报告了（1）证明者在 PIOP 和 PCS 组件上花费的时间（顶部），以及（2）这些成本的跨不同层类型的更细粒度分解（中部-底部）。我们观察到，正如预期，PCS 是证明者的主要瓶颈；然而，它并没有完全主导总运行时间。这主要归功于我们优化的类 GKR 的 PIOP，它减少了必须承诺的数据量，从而降低了承诺开销。另一个有趣的观察是，在 PIOP 和 PCS 中，超过 50% 的证明者成本来自激活函数、Softmax 和重新量化，其中后者整体上是最昂贵的组件，占证明者总时间的 27%。因此，优化这些组件中的任何一个都将大幅提升整体证明者性能。

> **English:**
> Breakdown of Prover Cost. Table 7 reports a detailed prover cost breakdown of DeepProve with BaseFold for GPT-2 and Gemma 3 inference at sequence length 512. Specifically, it reports (1) the prover time spent on the PIOP and PCS components (top), and (2) a finer-grained decomposition of these costs across different layer types (middle-bottom). We observe that, as expected, PCS constitutes the main bottleneck of the prover; however, it does not fully dominate the total runtime. This is primarily due to our optimized GKR-like PIOPs, which reduce the amount of data that must be committed, thereby lowering commitment overhead. Another interesting observation is that in both PIOP and PCS, more than 50% of the prover cost comes from activation functions, Softmax, and requantization, with the latter being the most expensive component overall, accounting for 27% of total prover time. Consequently, optimizing any of these components would yield substantial improvements in overall prover performance.

---

【表 7：序列长度 512、48 线程下见证承诺和 PIOP 时间（%）的逐层分解】
> **Table 7: Layer-wise breakdown of witness commitment and PIOP time (%) for sequence length 512 and 48 threads.**

```
                  GPT-2      Gemma 3
层               BaseFold    BaseFold
─────────────────────────────────────
证明者分解 (%)
 见证生成           5.97        5.51
 见证承诺          56.85       57.73
 打开               1.04        1.11
 PIOP             35.25       34.71
 其他               0.89        0.94
 总计                100         100
─────────────────────────────────────
见证承诺 (%)
 激活             22.96       10.21
 Logits            9.19       18.11
 重新量化         27.49       41.60
 RMS Norm         10.59       21.86
 Softmax          29.68        8.13
 重数(乘性)        0.08        0.04
 其他              0.01        0.05
 总计               100         100
─────────────────────────────────────
PIOP 分解 (%)
 激活             13.96        6.08
 加法              1.03        0.83
 注意力掩码        3.96        0.97
 Einsum           14.91       15.46
 嵌入              2.69        5.78
 Logits            7.83       16.10
 位置编码          0.06        0.83
 重新量化         27.30       29.96
 RMS Norm          6.02       15.24
 Softmax          19.39        5.40
 表                0.08        0.03
 聚合              1.79        1.95
 其他              0.98        1.37
 总计               100         100
```

**验证者时间与证明大小。** 验证者时间随序列长度略微增加，但即使在最大序列长度下，对 GPT-2 仍低于 3.2 秒，对 Gemma 3 低于 3.5 秒。我们的证明大小为 9–43 MiB，取决于模型、PCS 和序列长度。自然，HyperKZG 始终产生比 BaseFold 更小的证明。

> **English:**
> Verifier time and proof size. The verifier time marginally increases with the sequence length, but is still under 3.2 seconds for GPT-2 and under 3.5 seconds for Gemma 3, even at the maximum sequence length. Our proof sizes are between 9-43MiB depending on the model, PCS, and sequence length. Naturally, HyperKZG consistently produces smaller proofs than BaseFold.

**内存利用。** 表 2 也报告了方案的显存占用。总体而言，内存消耗随序列长度和模型大小增加，且它随模型大小线性扩展（如预期）。这与我们证明者的空间复杂度一致，后者与所证明的电路规模成正比。此外，由于 HKZG 使用更大的域，导致比 BaseFold 更高的内存利用率。具体来说，对评估的最大实例，我们的方案在 BaseFold 和 HKZG 下分别消耗 96 GB 和 147 GB RAM。事实上，后者无法在次机器上运行。这使得内存利用成为我们证明者的主要瓶颈（例如，证明超过 30 亿参数的模型，如 GPT-J，可能需要超过 1 TB RAM）。我们留下高效降低总内存利用（使用节省空间的 [PP24, PP25] 或基于磁盘的 [BMM⁺24] SNARK）作为有趣的未来方向。

> **English:**
> Memory Utilization. Table 2 also reports the memory utilization of our scheme. Overall, memory consumption increases with both the sequence length and, as expected, it scales linearly with the model size. This agrees with the space complexity of our prover which is proportional to the circuit size being proven. Furthermore, using HKZG results in higher memory utilization than BaseFold due to its larger field size. Concretely, for the largest evaluated instance, our scheme consumes 96GB and 147GB of RAM with BaseFold and HKZG respectively. In fact, the latter was not able to run in the secondary machine. This makes memory utilization the main bottleneck of our prover (e.g., proving models with more than 3 billion parameters, such as GPT-J, would likely require over a terabyte of RAM). We leave as an interesting future direction efficiently reducing total memory utilization using space-efficient [PP24, PP25] or disk-based [BMM+ 24] SNARKs.

**与先前工作的基线比较。** 对 DeepProve 与如 zkGPT [QSL⁺25] 和 zkTorch [CTK25] 等先前工作做基准测试的理想方法是，在所有工作中使用相同的硬件和语言模型。然而，这些工作的完整源代码并不可用。同样，zkLLM [SLZ24] 仅提供其注意力机制源码的访问；要将其扩展为完整的 LLM 推理证明系统需要大量工程努力。在这些约束下，类似先前工作 [QSL⁺25]，我们根据其论文中报告的数字比较 DeepProve 的性能，同时在表 3 中突出实验设置的差异。

> **English:**
> Baseline comparison with prior works. The ideal way to benchmark DeepProve's performance against prior works such as zkGPT [QSL+ 25] and zkTorch [CTK25] would be running all of them on the same hardware and language model. However, the full source code for these works is not available. Likewise, zkLLM [SLZ24] provides access only to their attention mechanism source code; this would require significant engineering effort to extend it to a full LLM inference proof system. Under these constraints, and similar to prior works [QSL+ 25], we compare DeepProve's performance based on the numbers reported in their papers, while highlighting the differences in the experimental setup, in Table 3.

【表 3：来自 [CTK25, QSL⁺25] 的基线吞吐量和硬件设置】
> **Table 3: Baseline throughput and hardware setup from [CTK25, QSL+ 25]. We exclude the comparison with zkLLM [SLZ24] since their performance numbers rely on GPU acceleration and are not replicable in CPU environments. TPM is tokens per minute and # Th is the number of threads used. Here RAM refers to maximum available system RAM, which is not necessarily the RAM used by the implementation. Neither zkGPT [QSL+ 25] nor zkTorch [CTK25] reports the actual RAM usage.**

| 工具 | 模型 | CPU | 系统 RAM | #Th | TPM |
|------|------|-----|---------|-----|-----|
| zkGPT [QSL⁺25] | GPT-2 | 2.6 GHz 16 核 | 200 GB | 32 | 2.75 |
| zkTorch [CTK25] | GPT-J | 2.6 GHz 32 核 | 4 TB | 64 | 0.09 |
| 我们 | GPT-2 | 2.9 GHz 24 核 | 504 GB | 48 | 127.23 |
| 我们 | GPT-2 | 4.2 GHz 16 核 | 128 GB | 32 | 175.08 |

**精度（Accuracy）。** 这里我们测试 DeepProve 实现的精度。回顾影响精度的主要变量是量化位长，因此我们报告三种不同位长（8、10、12）下的精度。表 4 报告了量化输出 logits 的精度统计，并与在 PyTorch 上运行的初始浮点模型的基线进行比较。我们通过运行前向传播，使用数据集中 25 个文档的前 256 个 token，计算 Wikitext-2 数据集上的困惑度（perplexity）。然后我们使用输入 token（移位 1 并省略第一个 token）作为真实标签，计算模型对这 256 个 token 输出的 logits 张量的前 255 行的困惑度得分，基线和 DeepProve 的量化引擎均如此。自然，更高的量化位长提供更好的困惑度度量（越低越好），且 DeepProve 接近基线。我们注意到，对 Gemma 3，任何低于 12 位量化的设置精度都很差。Gemma 3 中的激活值容易受到极大离群值的影响，且由于 Transformer 块中存在额外的 RMSNorm 层，传统的平滑技术无法应用。

> **English:**
> Accuracy. Here, we test the accuracy that DeepProve achieves. Recall that the main variable affecting this is the quantization bit-length, hence we report accuracy for three different lengths: 8, 10, and 12. Table 4, reports the accuracy statistics on the quantized output logits and compare them with the baseline from running the initial floating-point model on PyTorch. We compute the perplexity over the Wikitext-2 dataset by running a forward pass with the first 256 tokens of 25 documents in the dataset. We then compute the perplexity score of the first 255 rows of the logit tensor output by the model for these 256 tokens, using the input tokens (shifted by 1 and omitting the first token) as the ground truth, for both the baseline and DeepProve's quantized engine. Naturally, the higher quantization bit lengths provide better perplexity measures (lower is better), and DeepProve is close to the baseline. We note that for Gemma 3, anything below 12-bit quantization performs poorly for accuracy. The activations in Gemma 3 are susceptible to extremely large outliers, and due to the presence of additional RMSNorm layers in the transformer blocks, traditional smoothing techniques cannot be applied.

【表 4：DeepProve 量化推理引擎与基线浮点模型的精度比较】
> **Table 4: Accuracy of DeepProve's quantized inference engine compared to the baseline floating-point model.**

| 模型 | 量化位数 | 余弦相似度 | 困惑度(基线) | 困惑度(测试) |
|------|---------|-----------|-------------|-------------|
| GPT-2 | 8 | 0.9967 | — | 60.9273 |
| | 10 | 0.9993 | 49.2198 | 50.0806 |
| | 12 | 0.9966 | — | 49.4863 |
| Gemma 3 | 8 | 0.2035 | — | > 1150 |
| | 10 | 0.6998 | 515.46 | 1150.43 |
| | 12 | 0.9587 | — | 454.72 |

**量化对证明者性能的影响。** 针对上述三种量化级别，我们还研究了量化对证明者复杂度的影响。总体而言，将量化级别从 8 位增加到 12 位，仅增加 0.8% 的证明者时间开销。这是因为量化主要在用于证明激活和重新量化层的查找表数量增加时才影响证明者时间，而从 8 位增加到 12 位并非这种情况。简言之，我们的量化优化使我们能像使用 8 位量化一样快地进行证明，同时达到近乎完美的困惑度。

> **English:**
> Impact of quantization in prover performance. For the three quantization levels described above, we also investigated the impact of quantization in prover complexity. Overall, increasing the quantization level from 8 to 12 bits adds only a 0.8% additional overhead in prover time. This is because quantization mainly affects prover time when the number of lookup tables used to prove activation and re-quantization layers increase which is not the case when moving from 8 to 12 bits. Simply put, our quantization optimizations enable us to obtain as fast prover times as if we used 8 bit quantization while simultaneously attaining near-perfect perplexity.

**非幻觉证明（Proof of non-hallucination）。** 为展示我们非幻觉证明的性能，我们在 Fava Annotation 数据集 [MAB⁺24] 上进行了测试。如 [SBS⁺24] 所介绍，该数据集涵盖四个不同的数据源：从 Open Assistant 数据集 [ea23] 采样的知识密集型查询、来自 No Robots 数据集 [RTB⁺23] 的 Open QA 提示、指令遵循（Instruction-following）以及 WebNLG [GSNPB17] 数据集。它总共有 460 个样本。我们计算每个注意力层的注意力得分，并在正负结果数量相等的平衡数据集上呈现标准的基于阈值的检测指标，如准确率（Accuracy）和 ROC 曲线下面积（AUROC）。我们的实验与 [SBS⁺24] 的设置相同。我们的方案性能见表 5。我们报告所有层的最佳 AUROC 和准确率，这与 [SBS⁺24] 的做法一致。如表所示，我们量化后的 GPT-2 模型最高 AUROC 为 68.69%（L8 层），Gemma 3 为 65.84%（L18 层）；最高准确率对 GPT-2 为 67.96%，对 Gemma 3 为 64.16%。这些数字接近 [SBS⁺24] 报告的 Llama-2 和 Llama-3 幻觉检测算法的性能，证实了检测的有效性。

> **English:**
> Proof of non-hallucination. To demonstrate the performance of our proof of non-hallucination, we test it on the Fava Annotation dataset [MAB+ 24]. As introduced in [SBS+ 24], the dataset itself spans four different data sources: Knowledge-intensive queries sampled from the Open Assistant dataset [ea23], Open QA prompts from the No Robots dataset [RTB+ 23], Instruction-following and WebNLG [GSNPB17] datasets. It has 460 samples in total. We compute the attention score of each attention layer, and present standard threshold-based detection metrics such as Accuracy and Area under the ROC curve (AUROC) on balanced datasets with an equal number of positive and negative results. Our experiments are in the same setting as those in [SBS+ 24]. The performance of our scheme is presented in Table 5. We report the best AUROC and accuracy from all layers, which was done in [SBS+ 24]. As shown in the table, the highest AUROC is 68.69% (L8) in our quantized GPT-2 model and 65.84% (L18) in our quantized Gemma 3 model, while the highest accuracy is 67.96% for GPT-2 and 64.16% for Gemma 3. These numbers are close to the performance of the hallucination detection algorithm for Llama-2 and Llama-3 as reported in [SBS+ 24], confirming the effectiveness of the detection.

为理解量化的影响，我们还在从 HuggingFace [RWC⁺19, Tea25] 获取的、对应模型的浮点数上测试了检测算法。在原始 GPT-2 模型中，最高 AUROC 为 68.29%，最高准确率为 65.62%。量化后我们的性能略有提升。对于原始 Gemma 3 模型，最高 AUROC 为 70.5%，最高准确率为 65.87%。量化后性能略有下降。

> **English:**
> To understand the impact of our quantization, we also tested the detection algorithms on the corresponding models over floating-point numbers obtained from HuggingFace [RWC+ 19, Tea25]. In the original GPT-2 model, the highest AUROC is 68.29% and the highest accuracy is 65.62%. Our performance becomes slightly better after quantization. For the original Gemma3 model, the highest AUROC is 70.5% and the highest accuracy is 65.87%. The performance becomes slightly worse after quantization.

我们还评估了机器 1 上幻觉检测的证明者时间。因为它作为模型中的一个节点被集成，且见证与其他所有节点一起被承诺，我们只测量 sumcheck PIOP 的证明者时间。如表 5 所示，序列长度 512 的证明者时间仅为几十毫秒。该组件的开销只是可验证推理证明者时间的一小部分，如表 1 和表 2 所示。

> **English:**
> We also evaluate the prover time of the hallucination detection on machine 1. Because it is integrated as a node in the model, and the witnesses are committed together with all other nodes, we only measure the prover time of the sumcheck PIOP. As shown in Table 5, the prover time for sequence length 512 is only tens of milliseconds. The overhead of this gadget is only a tiny portion of the prover time of the verifiable inference, as shown in Tables 1 and 2.

【表 5：DeepProve 中非幻觉证明的性能（512 序列长度）】
> **Table 5: Performance of proof of non-hallucination in DeepProve–512 sequence length.**

| 模型 | AUROC (%) | 准确率 (%) | 证明 (ms) |
|------|-----------|-----------|----------|
| GPT-2 | 68.69 | 67.96 | 98.1 |
| Gemma 3 | 65.84 | 64.16 | 21.1 |

**分布式证明（Distributed proving）。** 由于搭建分布式环境需要不小的工程努力，我们使用次机器配置在 8 和 16 个工作器及一个协调者上模拟分布式 DeepProve 的性能。我们测量证明任意块的最大时间，并加上将每个单独证明传回协调者所需的时间（假设 1 Gb/s 的 LAN 连接）。这样，我们得到分布式设置的最坏情况性能。

> **English:**
> Distributed proving. We simulate the performance of distributed DeepProve on 8 and 16 workers and a coordinator using our secondary machine configuration as the engineering effort to set up a distributed environment is non-trivial. We measure the maximum time to prove any chunk and add the time to transfer each individual proof back to the coordinator, assuming a LAN connection at 1 Gb/s. In this way, we get the worst-case performance of the distributed setup.

表 6 报告了使用 8 个（顶部）和 16 个（底部）工作器、对 GPT-2 使用 BaseFold、序列长度范围 128–1024 的分布式证明者运行结果。总体而言，分布式证明显著提高了证明者时间和每个工作器的内存使用。首先，注意分布式证明允许用相同的计算资源证明更大的模型。例如，序列长度 512 的 GPT-2 在以显著吞吐量下被证明：8 个工作器 1079 TPM，16 个 1855 TPM——分别实现 6.74 倍和 10.64 倍的改进。同时，每个证明者所需的内存远少于非分布式。

> **English:**
> Table 6 reports our findings when running our distributed prover with 8 (top) and 16 (bottom) workers for GPT-2 with BaseFold, and sequence lengths ranging from 128-1024. Overall distributed proving significantly improves the prover time and memory usage of each worker. First, note that distributed proving allows proving bigger models with the same computational resources, e.g., GPT-2 at sequence length 512 is proven at a remarkable throughput of 1079TPM for 8 workers and 1855TPM for 16—achieving 6.74× and 10.64× improvement, respectively. At the same time, each prover needs significantly less memory than the non-distributed one.

【表 6：使用次机器配置的工作机器模拟分布式 DeepProve 的性能】
> **Table 6: Simulated performance of distributed DeepProve using worker machines with our secondary machine configuration. TPM denotes the throughput in tokens per minute, Single TPM is the single-machine throughput, and Speedup is TPM/Single TPM.**

| 机器数 | 模型 | PCS | 序列长度 | 上下文(min) | 证明(s) | 验证(s) | π(MiB) | TPM | Single TPM | 加速 |
|-------|------|-----|---------|------------|--------|--------|--------|-----|-----------|------|
| 8 (9) | GPT-2 | BaseFold | 128 | 9.83 <1.25 | 1.9 | 40.62 | 780.9 | 150.98 | 5.17× |
| | | | 256 | 14.58 | 2 | 41.9 | 1053.25 | 175.08 | 6.02× |
| | | | 512 | 28.45 | 2.11 | 43.28 | 1079.71 | 174.32 | 6.19× |
| | | | 1024 | 68.5 | 2.26 | 44.88 | 896.2 | — | — |
| 16 (17) | GPT-2 | BaseFold | 128 | 9.75 <1.24 | 2.15 | 50.39 | 788 | 150.98 | 5.22× |
| | | | 256 | 9.69 | 2.28 | 51.97 | 1585.4 | 175.08 | 9.05× |
| | | | 512 | 16.56 | 2.39 | 53.81 | 1855.1 | 174.32 | 10.64× |
| | | | 1024 | 39.5 | 2.5 | 55.49 | 1547.1 | — | — |

然而，最重要的是，我们的分布式证明者具有**最小的通信开销**。排除协调者分发轨迹的设置步骤，它只需一轮交互，总通信成本对应证明大小——8 个工作器为 43.2 MB，16 个为 53.81 MB。这使我们的系统适合 WAN 环境。实际上，对于带宽较低（100 Mb/s）的 WAN，DeepProve 在上下文长度 512、8 台机器下实现 1054 TPM——仅慢 2%！我们将分布式 DeepProve 的完整实现留作未来工作。

> **English:**
> Most importantly, however, our distributed prover has a minimal communication overhead. Excluding the setup step where the coordinator distributes the trace, it only requires a single round of interaction and the total communication cost corresponds to the proof size which is 43.2MB for 8 workers and 53.81MB for 16 workers. This makes our system suitable for WAN settings. Indeed, for a WAN with lower bandwidth (100 Mb/s), DeepProve achieves 1054TPM at context lengths 512 and 8 machines—only a 2% slowdown! We leave the full-fledged implementation of distributed DeepProve as future work.

---

## 6 相关工作（Related Work）

这里我们讨论关于密码学可验证机器学习（verifiable machine learning）的相关工作。与我们的工作正交，一些工作专注于证明神经网络的训练正确性 [APKP24]、逻辑回归 [GGJ⁺23] 和决策树 [PP24]，或证明凸损失算法到最优模型的接近性 [TGM⁺25]。接下来我们讨论更贴近我们工作的、专注于可验证推理（verifiable inference）的文献。

> **English:**
> Here we discuss related work on cryptographically verifiable machine learning. Orthogonal to ours, some works focus on proving training correctness for neural networks [APKP24], logistic regression [GGJ+ 23], and decision trees [PP24], or proving proximity to an optimal model for convex-loss algorithms [TGM+ 25]. We next discuss works focusing on verifiable inference which are more relevant to ours.

**"经典"机器学习模型（"Classical" ML models）。** 早期关于可验证推理的工作集中于更简单的机器学习模型（相对于 LLM），例如决策树 [WYX⁺21, ZFZS20, PP24] 和层数较少的经典神经网络 [KHSS22, FQZ⁺21]。随后的工作证明了卷积神经网络推理的正确性 [BFGV⁺23, LKKO24, LXZ21]。这些工作使用的技术多种多样，从在算术电路中编码 CNN 计算 [BFGV⁺23, LKKO24, ea25a]，到使用专门的 sum-check 协议 [LXZ21]——后者在精神上更接近我们的方法，尽管针对的是更简单的模型。另一条工作线专注于为可验证机器学习推理构建通用框架 [LWQ⁺24, CCYY25, MY25, zko24]。总的来说，这些工作以性能换取通用性，在证明 LLM 等大型模型时会产生显著的额外开销。

> **English:**
> "Classical" ML models. Early works on verifiable inference focused on simpler ML models (as opposed to LLMs), such as decision trees [WYX+ 21, ZFZS20, PP24] and classical neural networks with few layers [KHSS22, FQZ+ 21]. Subsequent works proved the correctness of convolutional neural network inference [BFGV+ 23, LKKO24, LXZ21]. The techniques used in these works range from encoding the CNN computation in an arithmetic circuit [BFGV+ 23, LKKO24, ea25a] to using specialized sum-check protocols [LXZ21] that are closer in spirit to our approach, albeit for simpler models. Another line of work focused on building generic frameworks for verifiable machine learning inference [LWQ+ 24, CCYY25, MY25, zko24]. Broadly, these works trade off performance for generality, and suffer significant overheads while proving large models such as LLMs.

**大语言模型推理（Large language model inference）。** 随着大语言模型的出现，有较少数量的工作提出了证明 LLM 推理正确性的技术 [CWSK24, SLZ24, CTK25, QSL⁺25]。ZKML 在算术电路中编码一个（略作修改的）GPT-2 版本，并使用 Halo2 [Zca20] 证明推理的正确性。不幸的是，这种"通用"方法在应用于 LLM 时会带来效率降低和巨大的架构需求。zkPyTorch [XLF⁺25] 采取类似的方法，但依赖于 GKR [GKR08]。随后的工作如 zkLLM [SLZ24]、zkGPT [QSL⁺25] 和 zkTorch [CTK25]，比 ZKML [CWSK24] 快一到两个数量级。

> **English:**
> Large language model inference. With the advent of large language models, a smaller number of works have proposed techniques to prove the correctness of LLM inference [CWSK24, SLZ24, CTK25, QSL+ 25]. ZKML encodes a (slightly modified) version of GPT-2 in an arithmetic circuit and uses Halo2 [Zca20] to prove the correctness of inference. Unfortunately, this "generic" approach leads in reduced efficiency and massive architecture requirements when applied to LLMs. zkPyTorch [XLF+ 25] takes a similar approach, but relies on GKR [GKR08]. Subsequent works such as zkLLM [SLZ24], zkGPT [QSL+ 25], and zkTorch [CTK25], are faster than ZKML [CWSK24] by one to two orders of magnitude.

zkLLM [SLZ24] 是第一个使用专门的 sum-check 协议高效证明注意力（attention）和 softmax 等 LLM 组件的工作。然而，该工作只专注于证明这些特定层，其概念验证实现为每一层返回单独的证明，而不断言层间传递的张量的正确性。从概念上讲，我们将他们证明 softmax 正确性的方法扩展到批量设定，并且我们的系统是端到端的。

> **English:**
> zkLLM [SLZ24] was the first work to efficiently prove LLM components such as attention and softmax using specialized sum-check protocols. However, that work only focuses on proving these specific layers and their proof-of-concept implementation returns individual proofs for each layer without asserting the correctness of tensors passed between them. Conceptually, we extend their approach for proving softmax correctness to the batch setting, and our system is end-to-end.

zkTorch [CTK25] 旨在通用性和专门化之间取得折中，通过将 LLM 分解为一组算子，这与我们的做法类似。然而，作者们专注于一次证明一个输出 token 的正确性。由于他们方法的通用性，如第 5 节所述，zkTorch 的性能通常低于 DeepProve。此外，他们报告的实验运行在一台拥有 64 核和 4 TB RAM 的机器上进行。该方法的一个最终缺点是，缩放因子的使用仅限于 2 的幂，这会比 DeepProve 中采用的灵活量化方法造成更大的精度损失。

> **English:**
> zkTorch [CTK25] aims for a middle ground between generality and specialization by decomposing LLMs into a set of operators, similar to what we do. However, the authors focus on generating proofs of correctness for one output token at a time. Due to the generality of their approach, as remarked in Section 5, the performance of zkTorch is generally lower than DeepProve. Moreover, their reported experimental runs on a machine with 64 cores and 4 TB RAM. A final drawback of that approach is that the use of scaling factors is limited to powers-of-2, which incurs greater precision loss than the flexible quantization approach adopted in DeepProve.

zkGPT [QSL⁺25] 是目前证明 LLM 推理的最新系统。然而，与上述工作一样，该系统只设计用于证明单个输出 token。此外，作者证明的是一个不包含残差连接、注意力掩码和最终投影层的、修改版 GPT-2 的正确性。这些放宽措施显著绕过了我们在第 1 节中讨论的离群值问题。与 zkTorch [CTK25] 类似，zkGPT 使用带缩放因子的定点乘法进行量化。虽然这种技术在证明上稍简单，但它带来一个权衡：主要缺点是，由于重新量化和非线性函数通常通过查找参数（lookup arguments）证明，使用不带截断（clamping）的固定缩放因子可能导致推理过程中生成的值过大而无法查找，从而迫使使用更大的查找表进行承诺。

> **English:**
> zkGPT [QSL+ 25] is currently the state-of-the-art system for proving LLM inference. However, like the above works, that system is only designed to prove a single output token. Moreover, the authors prove the correctness of a modified version of GPT-2 that doesn't contain residual connections, attention masks, and the final projection layer. These relaxations significantly circumvent the outlier issues that we discussed in Section 1. Similar to zkTorch [CTK25], zkGPT uses fixed-point multiplication with scaling factors for quantization. While this technique is slightly simpler to prove, it comes with a trade-off: The major downside is that since re-quantization and non-linear functions are usually proven via lookup arguments, using a fixed scaling factor without clamping can lead to situations where the values generated during inference are too large to be looked up, thus forcing a larger lookup table to commit.

最后，我们强调，我们是唯一提供使用 LLM 推理证明器**确切代码库**所得结果的准确率（accuracy）测量的工作。据我们所知，先前的工作没有将它们的精度实验与它们的证明代码库集成起来。如附录 C 所示，试图证明端到端 LLM 轨迹（例如重新量化、查找表大小选择、截断等）时会出现许多细微之处。因此，如果先前工作 [QSL⁺25, SLZ24, XLF⁺25] 在证明完整的 LLM 推理轨迹，它们是否能达到其报告的准确率结果并不完全清楚。

> **English:**
> Finally, we stress that ours is the only work to provide accuracy measurements for results achieved with the exact codebase of our LLM inference prover. To the best of our knowledge, prior works did not integrate their accuracy experiments with their proving codebase. As shown in Appendix C, many subtle nuances arise when trying to prove end-to-end LLM traces (such as requantization, lookup table size selection, clamping, etc.). Hence, it is not entirely clear whether prior works [QSL+ 25, SLZ24, XLF+ 25] would achieve their reported accuracy results if they were proving a full LLM inference trace.

---

## 附录 A  附加预备知识（Additional Preliminaries）

> ⚠️ **说明：**附录 A、B 为论文的正式密码学协议构造、定义、定理与严格证明（公式化的数学内容）。下列为该两附录各小节的中英对照标题与**关键叙事段落**的翻译；协议框、定义、定理与证明保持其数学形式（受 PDF 排版影响，原文公式已按语义还原）。完整的英文数学内容见于 `deepprove_text.txt` 对应小节。

**A.1 Sumcheck 协议（Sumcheck Protocol）** —— 由 Lund 等人 [LFKN90] 引入。它支持对多线性多项式在多布尔超立方体上的求和进行验证。
> **English:** The sumcheck protocol, introduced by Lund et al. [LFKN90] proceeds as follows...

**A.2 批量 Sumcheck（Batched Sumcheck）** —— 该定理给出 sumcheck 协议具有完美完备性，以及知识型逐轮（round-by-round）可靠性 κ<sub>rbr</sub> = ε<sub>rbr</sub> = max(h/|F|, h·m·d/|F|)，总证明者时间为 O(h·m·log m · n)，证明大小和验证时间为 O(h + m·d·log n)。
> **English:** Theorem 2. The sumcheck protocol for as described above has perfect completeness and (knowledge) round-by-round soundness ... Furthermore, the total prover time is O(h·m·log m · n) and proof size and verification time of O(h + m·d·log n).

**A.3 查找参数（Lookup Arguments）** —— 当表 T 可能呈指数级大但**可分解（decomposable）**时（例如现代证明者高效的 SNARK [HLP24, ...]），我们会修改该关系以支持可分解表。我们使用 [PH23] 提出的查找 PIOP，它先将 R_lkp 转换为 LogUp 方程 [Hab22]，随后使用 GKR 协议 [GKR08] 证明其正确计算。在我们的协议中，Lasso [STW23, AST24] 同样被用于证明激活函数的正确计算。
> **English:** In practice, T might be exponentially large but decomposable... In this work, we use the lookup PIOP proposed by [PH23] which first converts R_lkp to a LogUp equation [Hab22] and subsequently uses the GKR protocol [GKR08] to prove its correct computation... Lasso [STW23, AST24] and we will also use it in order to prove the correct computation of activation functions.

**A.4 交互式证明与多项式 IOP（Interactive Proofs & Polynomial IOPs）**

**A.5 知识论证（Arguments of Knowledge）** —— 定义了零知识简洁非交互式论证（zk-SNARK）及其完备性、可靠性、简洁性等性质（定义 6-8）。
> **English:** Definition 6 ([CCC+ 25]). A zero-knowledge succinct non-interactive... Definition 7 (Completeness)... Definition 8 (Succinctness)...

**A.6 多项式承诺（Polynomial Commitments）** —— 定义多项式承诺方案（PCS）。论文采用的编译将 PIOP 的所有预言替换为多项式承诺。
> **English:** Definition 12 (Polynomial Commitment Scheme). A multi-linear polynomial commitment scheme (PCS)...

## 附录 B  算子证明（Operator Proving）

**B.1 广义矩阵乘法（Generalized Matrix Multiplication）** —— 协议 4 是 R_MatMul 的一个 PIOP（定理 6）。我们此处使用的 PIOP 首先由 Thaler [Tha13] 提出，它依赖 sumcheck 协议来证明矩阵乘法的正确性。
> **English:** ...the PIOP we use in this work we use the PIOP first proposed by Thaler [Tha13] relies on the sumcheck protocol to prove the correctness of matrix multiplication.

**B.2 重新量化与 GeLU（Requantization & GeLU）**

**B.2.1 将重新量化与 GeLU 合并（Merging requantization with GeLu）** —— 首先注意 GeLu 函数以向量 x ∈ Fⁿ 为输入并输出 y ← GeLu(x)，其中 y[i] = x[i]·P[X ≤ x[i]]，P[X ≤ x[i]] 是标准高斯分布的累积分布函数。为证明上述函数，我们考虑两种情形：(1) x ∈ Fⁿ 的所有元素为 q 位数字；(2) x ∈ Fⁿ 的元素为 q′ 位或更大，其中 q′ > q。在第一种情形下，我们对一个表 T_GeLu ∈ F^(2^q) 应用标准查找 PIOP；在第二种情形下，我们使用重新量化的查找参数。
> **English:** B.2.1 Merging requantization with GeLu. First, note that the GeLu function takes as input a vector x ∈ F^n and outputs y ← GeLu(x) where y[i] = x[i]·P[X ≤ x[i]] where P[X ≤ x[i]] is the cumulative distribution function of the standard Gaussian distribution. To prove the above function we consider two cases: (1) all elements of x ∈ F^n are q-bit numbers... and (2) elements of x ∈ F^n are q'-bit numbers or even larger where q' > q...

**B.3 嵌入层（Embedding）** —— 本节提供嵌入层 PIOP 的更多细节。所要证明的关系是：(x; w) = x, [[f_T]], [[f_Y]]; (T, Y) : Y = Embed(T, x)。其证明者时间为 O((t + s)·d) 次域运算，证明大小和验证时间为 O(log t)。
> **English:** B.3 Embedding. In this section we provide more details about the PIOP for the embedding layer. Not the relation we try to prove is... It has prover time of O((t + s)·d) field operations and proof size and verification time of O(log t).

**B.4 位置编码（Positional Encodings)** —— 位置编码是 LLM 中用于让词序生效的关键组件。我们专注于证明两种最常用的位置技术：(1) 绝对位置嵌入与 (2) 旋转位置嵌入（RoPE，见 §4.3.1）。
> **English:** B.4 Positional Encodings. Recall that positional encodings are key components in LLMs used to enable order in words. In this work, we will focus on proving the correctness of the two most used positional techniques: (1) Absolute... and (2) Rotary position encodings...

**B.5 Argmax** —— 协议 9 是 R_argmax 的一个 PIOP（定理 11）。该协议在 GPT-2（图 1）与 Gemma 3（图 4）末层证明 argmax，为执行提供确定性。
> **English:** B.5 Argmax. Theorem 11. Protocol 9 is a PIOP for R_argmax with κ_rbr = ε_rbr = ... The argmax in the final layer of GPT-2 and Gemma 3 provides determinism to the execution.

**B.6 RMSNorm / LayerNorm** —— 见下一节正文与附录 C 的精度分析。

**B.7 Softmax 与批量证明（Softmax & Batch Proving）** ——

**B.7.1 批量证明 Softmax 层（Batch Proving Softmax Layers）** —— 证明者可用批处理方式一次性证明多个 softmax 实例，从而共享承诺、降低总开销。
> **English:** B.7.1 Batch Proving Softmax Layers. As we will see next, the prover...

**B.8 多头注意力层（Multi-Head Attention Layer）** ——

**B.8.1 单头注意力层的一个 PIOP** —— 单头注意力关系定义为 R_SHA，其输入为投影后的查询 Q、键 K、值 V。
> **English:** B.8.1 A PIOP for Single-Head Attention Layer. Recall that a single...

**B.8.2 带掩码的单头注意力层的一个 PIOP** —— 该协议引入注意力掩码（Attention Mask），用于认证推理（保证第 t 个 token 的计算不使用第 t+1 个 token 的值）。
> **English:** B.8.2 A PIOP for Single-Head Attention Layer with Masking. In the...

**B.8.3 多头注意力层的一个 PIOP** —— 附录 B.8.3 给出批处理 ℎ 个单头实例来证明 MHA 层的完整协议（协议 16，定理 15）。
> **English:** B.8.3 A PIOP for Multi-Head Attention Layer. We give a detailed...

**B.9 非幻觉证明构件（Proof of Non-Hallucination Gadget）** —— 在大语言模型领域，幻觉检测的已有方法主要是基于语义熵（semantic entropy）的启发式方法 [FKKG24] 或基于内部表征（internal states）的方法 [GCX+ 25, OTG+ 25]。LLM-Check [SBS⁺24] 首次提出可证明的基于注意力的检测。协议由两个 PIOP 组成：(1) 证明 Ker(Q,K) 矩阵计算正确；(2) 计算注意力得分 score = Σ P·log2(sf_X·X[j,i])，最后利用查找 PIOP 证明对数运算。
> **English:** B.9 Proof of Non-Hallucination Gadget. (1) ... (2) In the second PIOP, we compute the attention score. Given the input sequence... score = Σ_{j∈[s],i∈[h]} P · log2(sf_X · X[j,i]) ... (3) Finally, we leverage a lookup PIOP to prove the logarithmic operations.

## 附录 C  关于精度与性能的细节（Details Related to Accuracy and Performance）

> ⚠️ **说明：**该附录不在正文中出现，而是论文技术细节的"支撑材料"，讨论各个算子（重新量化、截断、RMSNorm、LayerNorm）在证明中的具体精度处理。以下为该附录关键叙事与策略的中英对照翻译；其中的协议方程与引理保持数学形式。

### C.1 重新量化的证明（Proving Requantization）

为将查找表大小降至最小，DeepProve 先乘以定点乘子 ε·2ᶠ，加上一个舍入常数，然后将输入分解为若干块作为查找表的输入。这些块对应输出值或被右移丢弃的部分。不失一般性，我们假设分解使得每块值都是 k 位整数。右移大小为 f+n，因此我们有 t := ⌈(f+n)/k⌉ 个被移走的块。证明者与验证者检查方程 ε·2ᶠ·In(x̄) + 2^(f+n−1) = 2^(f+n)·Out(x̄) + Σ_{i=1}^t 2^(k(i−1))·Shiftedᵢ(x̄)。对 i ∈ [1, t−1]，查找参数用于执行范围检查以强制 Shifted(X̄) ∈ [0, 2ᵏ − 1]；对 i = t，则强制 2^(k−r)·Shiftedₜ(x̄) ∈ [0, 2ᵏ − 1]。
> **English:** C Proving Requantization: To keep lookup table size to a minimum, DeepProve first multiplies by a fixed point multiplier ε·2^f, adds a rounding constant and then decomposes the input into chunks to be passed as the input to lookup tables. These chunks correspond to either the output value or the parts that are right-shifted away. ... we decompose in such a way that the values in each chunk of the decomposition are k-bit integers. As before the size of the right shift is f+n, thus we have t := ⌈(f+n)/k⌉ chunks that are shifted away. ... the prover and verifier check the equation ε·2^f·In(x̄) + 2^(f+n−1) = 2^(f+n)·Out(x̄) + Σ_{i=1}^t 2^(k(i−1))·Shiftedᵢ(x̄). ... for i ∈ [1, t−1] the lookup argument is used to perform a range check enforcing that Shifted(X) ∈ [0, 2^k − 1] and when i = t it enforces 2^(k−r)·Shiftedₜ(x̄) ∈ [0, 2^k − 1].

### C.2 截断的证明（Proving clamping）

上一节描述了如何证明重新缩放与右移，但 Out(x̄) 的值并不能保证是 k 位整数，因此必须被可证地截断。最简单的办法是再加一个查表，它输入 Out(x̄) 并输出 Clamp(x̄)。但实际上这个截断查找表太大，因此 DeepProve 根据值是否预期有符号，采用两种不同方法。**无符号截断**时，重新缩放后的输入被进一步分解，配合多线性多项式 ZeroInⱼ/ZeroOutⱼ 通过 Sumcheck 验证；**有符号截断**时，额外引入 SgnIn(x̄)。
> **English:** C Proving clamping. The previous section describes how to prove the rescaling and the right shift, but the values of Out(x̄) are not guaranteed to be k-bit integers and so they must be provably clamped. The simplest solution would be to have another lookup table that takes as input Out(x̄) and outputs Clamp(x̄). In practice though this clamping lookup table is too large and so DeepProve uses two different approaches based on whether the value is expected to be signed or not. Unsigned clamping... Signed Clamping...

### C.3 RMSNorm 与 LayerNorm 的精度（Accuracy of RMSNorm/LayerNorm）

为标准地证明 RMSNorm，需对 X_q 逐元素平方、用查找参数证明除以 n、开方与求逆，再乘上逆平方根张量。这有两个问题：在逆平方根查找时产生精度损失，且该层输出紧接着必须经历重新量化过程。为尽可能保留精度，我们采用我们重新量化过程的一个适配版本，然后强制 RMSNorm 的输出满足某个误差界。以引理 1、引理 3（LayerNorm 界）与引理 4 保证：若 ρ 与各缩放因子满足相应不等式，则输出有界、语义正确。此外，LayerNorm 可被视为"先行减去每行均值再执行 RMSNorm"，即 LayerNorm(X) = RMSNorm(X − μ)。

> **English:** C RMSNorm/LayerNorm accuracy. If we were to prove this normally we would compute the elementwise square of X_q, pass X_q² to a lookup argument to prove the division by n, square root and inversion, and then multiply X_q elementwise by this inverse square root tensor. This has two problems, it incurs accuracy loss during the inverse square root lookup and the output of the layer has to undergo the requantization procedure immediately afterwards. Instead to retain as much accuracy as possible we utilise an adapted version of our requantization procedure and then enforce that the output of the RMSNorm satisfies a certain error bound. ... Lemma 1, Lemma 3, Lemma 4 ... In fact the LayerNorm operation can be viewed as an RMSNorm where the mean of each row is subtracted first, that is LayerNorm(X) = RMSNorm(X − μ).

### C.4 尽可能避免重新量化（Avoid requantization when possible）

即使两个 8 位量化整数相乘的输出产生 16 位整数，在某些场景下也是可接受的。一个 N×M 矩阵乘以 M×P 矩阵最多能产生范围为 B = 2·q + log₂(M) + 1 位的整数。如果 B < |F|，或足够小而仍可在非线性层的定点乘法中使用，那么就没有必要在之后立即重新量化。重新量化会带来精度损失，因此避免它能提高精度。在使用大域 F 进行证明时，这一策略尤其有效。
> **English:** C Avoid requantization when possible. Even if the output of a multiplication between two 8-bit quantized integer gives a 16-bit integer, this may be acceptable in some scenario. A matrix N×M matrix multiplied by a M×P matrix can yield at maximum an integer of range B = 2∗q + log2(M) + 1 bits. If B < |F| or is sufficiently small to still be used during fixed point multiplication for non linear layers, then there is no need to requantize right after. Requantizing incurs accuracy loss and thus avoiding it increases the accuracy. This strategy is even more effective when using a large field F during proving.

---

## 参考文献（References）

> **说明：**参考文献为标准学术引文，按惯例保留英文原文格式。

- [AKKP25] Deema Alnuhait, Neeraja Kirtane, Muhammad Khalifa, and Hao Peng. Factcheckmate: Preemptively detecting and mitigating hallucinations in lms, 2025.
- [AMC⁺24] Saleh Ashkboos, Amirkeivan Mohtashami, Maximilian L. Croci, Bo Li, Pashmina Cameron, Martin Jaggi, Dan Alistarh, Torsten Hoefler, and James Hensman. Quarot: Outlier-free 4-bit inference in rotated llms, 2024.
- [APKP24] Kasra Abbaszadeh, Christodoulos Pappas, Jonathan Katz, and Dimitrios Papadopoulos. Zero-Knowledge Proofs of Training for Deep Neural Networks. In ACM CCS, 2024.
- [AST24] Arasu Arun, Srinath Setty, and Justin Thaler. Jolt: SNARKs for Virtual Machines via Lookups. In EUROCRYPT, 2024.
- [BCS16] Eli Ben-Sasson, Alessandro Chiesa, and Nicholas Spooner. Interactive oracle proofs. In TCC, pages 31–60, 2016.
- [BFGV⁺23] David Balbás, Dario Fiore, Maria Isabel González Vasco, Damien Robissout, and Claudio Soriente. Modular Sumcheck Proofs with Applications to Machine Learning and Image Processing. In ACM CCS, 2023.
- [BMM⁺24] Anubhav Baweja, Pratyush Mishra, Tushar Mopuri, Karan Newatia, and Steve Wang. Scribe: Low-memory snarks via read-write streaming. IACR Cryptol. ePrint Arch., 2024.
- [FKKG24] Sebastian Farquhar, Jannik Kossen, Lorenz Kuhn, and Yarin Gal. Detecting hallucinations in large language models using semantic entropy. Nature, pages 625–630, 2024.
- [FQZ⁺21] Boyuan Feng, Lianke Qin, Zhenfei Zhang, Yufei Ding, and Shumo Chu. ZEN: An optimizing compiler for verifiable, zero-knowledge neural network inferences. Cryptology ePrint Archive, 2021.
- [GCX⁺25] Cheng Gao, Huimin Chen, Chaojun Xiao, Zhiyi Chen, Zhiyuan Liu, and Maosong Sun. H-neurons: On the existence, impact, and origin of hallucination-associated neurons in llms, 2025.
- [GJJZ22] Sanjam Garg, Abhishek Jain, Zhengzhong Jin, and Yinuo Zhang. Succinct Zero Knowledge for Floating Point Computations. In ACM CCS, 2022.
- [GKR08] Shafi Goldwasser, Yael Tauman Kalai, and Guy N. Rothblum. Delegating computation: interactive proofs for muggles. In STOC, 2008.
- [GSNPB17] Claire Gardent, Anastasia Shimorina, Shashi Narayan, and Laura Perez-Beltrachini. The WebNLG challenge: Generating text from RDF data. In Proceedings of the 10th International Conference on Natural Language Generation, pages 124–133, 2017.
- [Hab22] Ulrich Haböck. Multivariate lookups based on logarithmic derivatives. Cryptology ePrint Archive, 2022.
- [HCL⁺24] Meng Hao, Hanxiao Chen, Hongwei Li, Chenkai Weng, Yuan Zhang, Haomiao Yang, and Tianwei Zhang. Scalable Zero-knowledge Proofs for Non-linear Functions in Machine Learning. In USENIX Security, 2024.
- [HLP24] Ulrich Haböck, David Levit, and Shahar Papini. Circle STARKs. Cryptology ePrint Archive, 2024.
- [JKC⁺18] Benoit Jacob, Skirmantas Kligys, Bo Chen, Menglong Zhu, Matthew Tang, Andrew G. Howard, Hartwig Adam, and Dmitry Kalenichenko. Quantization and Training of Neural Networks for Efficient Integer-Arithmetic-Only Inference. In CVPR, 2018.
- [KHSS22] Daniel Kang, Tatsunori Hashimoto, Ion Stoica, and Yi Sun. Scaling up Trustless DNN Inference with Zero-Knowledge Proofs, 2022.
- [LFKN90] Carsten Lund, Lance Fortnow, Howard J. Karloff, and Noam Nisan. Algebraic Methods for Interactive Proof Systems. In FOCS, 1990.
- [LKKO24] Seunghwa Lee, Hankyung Ko, Jihye Kim, and Hyunok Oh. vCNN: Verifiable Convolutional Neural Network Based on zk-SNARKs. In IEEE TDSC, 2024.
- [LXW⁺24] Haokun Lin, Haobo Xu, Yichen Wu, Jingzhi Cui, Yingtao Zhang, Linzhan Mou, Linqi Song, Zhenan Sun, and Ying Wei. Duquant: Distributing outliers via dual transformation makes stronger quantized llms, 2024.
- [LXZ21] Tianyi Liu, Xiang Xie, and Yupeng Zhang. zkCNN: Zero Knowledge Proofs for Convolutional Neural Network Predictions and Accuracy. In ACM CCS, 2021.
- [MAB⁺24] Abhika Mishra, Akari Asai, Vidhisha Balachandran, etc. Fine-grained hallucination detection and editing for language models. In First Conference on Language Modeling, 2024.
- [OTG⁺25] Hadas Orgad, Michael Toker, Zorik Gekhman, Roi Reichart, Idan Szpektor, Hadas Kotek, and Yonatan Belinkov. LLMs know more than they show: On the intrinsic representation of LLM hallucinations. In ICLR, 2025.
- [PH23] Shahar Papini and Ulrich Haböck. Improving logarithmic derivative lookups using GKR. Cryptology ePrint Archive, 2023.
- [PP24] Christodoulos Pappas and Dimitrios Papadopoulos. Sparrow: Space-Efficient zkSNARK for Data-Parallel Circuits and Applications to Zero-Knowledge Decision Trees. In ACM CCS, 2024.
- [PP25] Christodoulos Pappas and Dimitrios Papadopoulos. Hobbit: Space-Efficient zkSNARK with Optimal Prover Time. In USENIX Security '25, 2025.
- [QSL⁺25] Wenjie Qu, Yijun Sun, Xuanming Liu, Tao Lu, Yanpei Guo, Kai Chen, and Jiaheng Zhang. zkgpt: An efficient non-interactive zero-knowledge proof framework for LLM inference. In USENIX Security, pages 2045–2063, 2025.
- [RSZ25] Rahul Raman, Khushi Sharma, and Sai Qian Zhang. Rethinking the Outlier Distribution in Large Language Models, 2025.
- [RTB⁺23] Nazneen Rajani, Lewis Tunstall, Edward Beeching, Nathan Lambert, Alexander M. Rush, and Thomas Wolf. No robots. 2023.
- [RWB⁺25] Arman Riasi, Haodi Wang, Rouzbeh Behnia, Viet Vo, and Thang Hoang. Zero-Knowledge AI Inference with High Precision. In ACM CCS, 2025.
- [RWC⁺19] Alec Radford, Jeffrey Wu, Rewon Child, David Luan, Dario Amodei, Ilya Sutskever, et al. Language models are unsupervised multitask learners. OpenAI blog, 2019.
- [SBS⁺24] Gaurang Sriramanan, Siddhant Bharti, Vinu Sankar Sadasivan, Shoumik Saha, Priyatham Kattakinda, and Soheil Feizi. Llm-check: Investigating detection of hallucinations in large language models. NeurIPS, 2024.
- [SLP⁺21] Jianlin Su, Yu Lu, Shengfeng Pan, Ahmed Murtadha, Bo Wen, and Yunfeng Liu. RoFormer: Enhanced Transformer with Rotary Position Embedding, 2021.
- [SLZ24] Haochen Sun, Jason Li, and Hongyang Zhang. zkLLM: Zero Knowledge Proofs for Large Language Models. In ACM CCS, pages 4405–4419, 2024.
- [SSZ21] Suraj Subramanian, Mark Saroufim, and Jerry Zhang. Quantization in practice, 2021.
- [STW23] Srinath Setty, Justin Thaler, and Riad Wahby. Unlocking the lookup singularity with Lasso. Cryptology ePrint Archive, 2023.
- [TGM⁺25] Gefei Tan, Adria Gascon, Sarah Meiklejohn, Mariana Raykova, Xiao Wang, and Ning Luo. Founding Zero-Knowledge Proof of Training on Optimum Vicinity. In ACM CCS, 2025.
- [Tha13] Justin Thaler. Time-Optimal Interactive Proofs for Circuit Evaluation. In CRYPTO, 2013.
- [Tha25] Justin Thaler. Sum-check Is All You Need, 2025.
- [VSP⁺17] Ashish Vaswani, Noam Shazeer, Niki Parmar, Jakob Uszkoreit, Llion Jones, Aidan N Gomez, Łukasz Kaiser, and Illia Polosukhin. Attention is All you Need. In NeurIPS, 2017.
- [WYX⁺21] Chenkai Weng, Kang Yang, Xiang Xie, Jonathan Katz, and Xiao Wang. Mystique: Efficient Conversions for Zero-Knowledge Proofs with Applications to Machine Learning. In USENIX Security, 2021.
- [XLF⁺25] Tiancheng Xie, Tao Lu, Zhiyong Fang, Siqi Wang, Zhenfei Zhang, Yongzheng Jia, Dawn Song, and Jiaheng Zhang. zkPyTorch: A hierarchical optimized compiler for zero-knowledge machine learning. Cryptology ePrint Archive, 2025.
- [ZCF24] Hadas Zeilberger, Binyi Chen, and Ben Fisch. BaseFold: Efficient Field-Agnostic Polynomial Commitment Schemes from Foldable Codes. In CRYPTO, 2024.
- [ZFZS20] Jiaheng Zhang, Zhiyong Fang, Yupeng Zhang, and Dawn Song. Zero Knowledge Proofs for Decision Tree Predictions and Accuracy. In ACM CCS, 2020.
- [zko24] zkonduit. EZKL: Easy zero knowledge library. https://github.com/zkonduit/ezkl, 2024.
- [ZPM⁺24] Muru Zhang, Ofir Press, William Merrill, Alisa Liu, and Noah A. Smith. How language model hallucinations can snowball. In ICML, 2024.

---

*本文件为 deepprove.pdf 的中英逐段对照翻译。正文部分（§摘要 至 §6 相关工作）采用"中文段落 + 英文原文块引用"的逐段对照格式；附录 A/B/C 因包含大量正式化协议与证明，采用"中英对照标题 + 关键叙事段落"的结构化翻译，完整英文学术内容请参照源 PDF 或 `deepprove_text.txt`。*
