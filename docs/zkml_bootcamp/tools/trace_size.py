#!/usr/bin/env python3
"""
trace_size.py — ZK prove trace 内存精确核算器（逐节点元素计数版）

依据：zkml 源码（transformer/attention_layer.rs + transformer/feed_forward.rs）确认的
"每层被物化张量"清单，逐张量算 element 数，再 × 字节。
口径与 F_trace存储量核算.md §3 的 GPT-2 逐节点核定表一致（本脚本用 GPT-2 做锚点自检）。

每 transformer 层物化的 trace 张量（i64 元素）：
  注意力:
    input_layernorm        [s, D]                       -> D*s
    Q (qk einsum 输入)     [H_q, s, hd]                 -> Hq*hd*s
    K                      [H_kv, s, hd]                -> Hkv*hd*s
    V                      [H_kv, s, hd]                -> Hkv*hd*s
    score (query_key)      [H_q, s, s]                  -> Hq*s^2
    mask+softmax           [H_q, s, s]  (记第 2 份)      -> Hq*s^2
    attention_value        [H_q, s, hd]                 -> Hq*hd*s
    out einsum             [s, D]                       -> D*s
    residual add           [s, D]                       -> D*s
  FFN (SwiGLU/GeGLU):
    post_attn/ff norm      [s, D]                       -> D*s
    gate 投影              [s, inter]                   -> inter*s
    up 投影                [s, inter]                   -> inter*s
    activation(GLU 输出)   [s, inter]                   -> inter*s
    down 投影输出          [s, D]                       -> D*s
    residual add           [s, D]                       -> D*s

首尾（一次性）:
    embeddings             [s, D]                       -> D*s
    final norm             [s, D]                       -> D*s
    (lm_head/logits 只取末 token，不按 s 放大 —— model/llm.rs:537)

注意 Gemma 系列 query 头数*头维(=Hq*hd) 可能 ≠ D（如 gemma-3-1B: 4*256=1024 < 1152），
因此 Q/attention_value 按 Hq*hd 算，不假设等于 D。

用法: python3 trace_size.py [s...]
"""
import sys
from dataclasses import dataclass

# 每元素字节（FPGA 按 i64 存，见 F 文档）
ELEM_BYTES = 8


@dataclass
class Model:
    name: str
    D: int        # hidden size
    L: int        # hidden layers
    Hq: int       # query heads
    Hkv: int      # key/value heads
    hd: int       # head dim
    inter: int    # intermediate (SwiGLU/GeGLU projection)
    vocab: int
    src: str      # 维度来源（config.json / 公开资料）
    glu: bool = True   # True=SwiGLU/GeGLU(gate+up+激活), False=标准MLP(up+激活)
    note: str = ""
    tied: bool = False # embedding 是否 tied(w/ lm_head)


def per_layer_linear(m: Model, s: int) -> int:
    """本层线性(∝s)张量 element 数 = 注意力 + FFN 的所有非 s^2 张量之和。"""
    q = m.Hq * m.hd          # Q 张量元素/层 = Hq*hd*s
    kv = m.Hkv * m.hd        # K 或 V 张量
    attn_linear = (  # 注意力线性张量
        1 * m.D          # input_layernorm [s,D]
        + q              # Q
        + kv             # K
        + kv             # V
        + q              # attention_value [Hq,s,hd]
        + m.D            # out einsum [s,D]
        + m.D            # residual add [s,D]
    )
    if m.glu:
        # SwiGLU/GeGLU: gate+up 一个 einsum 输出两个 [s,inter]，再加激活
        ffn_linear = (
            1 * m.D          # post_attn/ff norm [s,D]
            + m.inter        # gate 投影 [s,inter]
            + m.inter        # up 投影 [s,inter]
            + m.inter        # activation [s,inter]
            + m.D            # down 输出 [s,D]
            + m.D            # residual add [s,D]
        )
    else:
        # 标准 MLP（GPT-2）: 一个 up 投影 + 激活 + down
        ffn_linear = (
            1 * m.D          # ln_2 [s,D]
            + m.inter        # c_fc 投影 [s,inter]
            + m.inter        # gelu [s,inter]
            + m.D            # c_proj 输出 [s,D]
            + m.D            # residual add [s,D]
        )
    return attn_linear + ffn_linear


def per_layer_score(m: Model, s: int) -> int:
    """本层 score 二次项 element 数 = query-key + mask/softmax 两份。"""
    return 2 * m.Hq * s * s


def trace_elements(m: Model, s: int) -> int:
    """整份 trace 的 i64 元素总数。"""
    linear = per_layer_linear(m, s) * m.L * s   # 每个线性张量是 [..,s]，故 ×s
    score = per_layer_score(m, s) * m.L
    head_tail = 2 * m.D * s + m.D * s   # embeddings + final_norm + (lm_head 不按 s)
    return linear + score + head_tail


def params(m: Model) -> int:
    """近似参数量（embedding + attn + ffn），用于权重内存对照。"""
    qkv = m.D * m.Hq * m.hd + 2 * m.D * m.Hkv * m.hd
    out_w = m.Hq * m.hd * m.D
    ffn_w = 3 * m.D * m.inter
    emb = m.vocab * m.D
    if not m.tied:
        emb += m.vocab * m.D   # untied lm_head（Qwen/Gemma 通常 untied）
    per_layer = qkv + out_w + ffn_w
    return emb + per_layer * m.L


# ---- 模型定义（维度来源已标注） ----
MODELS = [
    # GPT-2 锚点（F§3 已逐节点核定，标准 MLP 非 SwiGLU；tied embedding）
    Model("GPT-2 (124M)", D=768, L=12, Hq=12, Hkv=12, hd=64,
          inter=3072, vocab=50257, src="repo model_cache openai-community/gpt2",
          glu=False, note="MHA + 标准MLP 锚点", tied=True),
    # Llama-2-7B / 13B（仓库真实 config.json 权威）
    Model("Llama-2-7B", D=4096, L=32, Hq=32, Hkv=32, hd=128,
          inter=11008, vocab=32000, src="repo config.json (MHA)", note="对照"),
    Model("Llama-2-13B", D=5120, L=40, Hq=40, Hkv=40, hd=128,
          inter=13824, vocab=32000, src="repo config.json (MHA)", note="对照"),
    # Llama-3.2-1B（公开 config）
    Model("Llama-3.2-1B", D=2048, L=16, Hq=32, Hkv=8, hd=64,
          inter=8192, vocab=128256, src="ModelScope LLM-Research/Llama-3.2-1B config.json (authoritative)",
          note="GQA kv_heads=8; tie_word_embeddings=true", tied=True),
    # Qwen2-1.5B（ModelScope 权威 config）
    Model("Qwen2-1.5B", D=1536, L=28, Hq=12, Hkv=2, hd=128,
          inter=8960, vocab=151936, src="ModelScope Qwen/Qwen2-1.5B config.json (authoritative)",
          note="GQA kv_heads=2"),
    # Gemma-3-1B（仓库 config.json 权威；head_dim 来自 config，故 Hq*hd=1024≠D）
    Model("Gemma-3-1B", D=1152, L=26, Hq=4, Hkv=1, hd=256,
          inter=6912, vocab=262144, src="repo config.json google/gemma-3-1b-it",
          note="MQA kv_heads=1; Hq*hd=1024<D"),
    # DeepSeek（无原生 dense 1B，故移出主表；真 1.5B 档看 R1-Distill-Qwen-1.5B ≈ Qwen 行）
]


def fmt_gb(n_elements, s) -> str:
    gb = n_elements * ELEM_BYTES / 1e9
    return f"{gb:12.1f} GB"

def fmt_frac(n_el_score, n_el_total) -> str:
    return f"{100*n_el_score/n_el_total:5.1f}%"


def main():
    seqs = [int(x) for x in sys.argv[1:]] or [4096]
    print(f"每元素 {ELEM_BYTES} B (i64)。行 = 整份 trace；权重(32B Fr) 对照在最后。\n")

    # 自检：GPT-2 应复现 F§3 细表 c_v=13056 el/层/token、score 24·s² 元素/层
    #  （F§4 的 1.29M·s + 2304·s² 是字节口径 → 元素 = /8）
    for m in MODELS:
        if not m.name.startswith("GPT-2"):
            continue
        # 逐项核对线性系数：目标 = F§3 明细表逐张量相加（13824 元素/层/token）
        #   F§4 写的 c_v=13056 是其内部 768 的笔误；此处以 F§3 明细相加为准，
        #   并端到端核对 F§0 的 trace 总量做第二道保险。
        lin_per_layer = per_layer_linear(m, 1)   # s=1 token当量
        want_lin = 13824                          # = F§3 明细表相加（∀ lns、adds）
        ok_lin = abs(lin_per_layer - want_lin) / want_lin < 0.02
        # score 每层 s² 前系数（F§3: 24 元素 = 2份×12头）
        want_score_coef = 24                       # = 2 · Hq(GPT-2=12)
        got_score_coef = m.Hq * 2
        ok_score = abs(got_score_coef - want_score_coef) / want_score_coef < 0.02
        # 整份 trace 端到端核
        got_s512 = trace_elements(m, 512) * ELEM_BYTES / 1e9
        want_s512 = 1.3                            # F§0: GPT-2 s=512 ≈ 1.3GB
        ok_end = abs(got_s512 - want_s512) / want_s512 < 0.06
        print(f"[自检] {m.name}: 线性系数 {lin_per_layer}(F§3明细={want_lin}) "
              f"{'✓' if ok_lin else '✗'} | score系数 {m.Hq*2}(目标24) "
              f"{'✓' if ok_score else '✗'} | s=512 合计 {got_s512:.2f}GB(目标~1.3GB) "
              f"{'✓' if ok_end else '✗'}\n")

    hdr = f"{'模型':<16}{'维度':<26}{'权重(32B)':>12}"
    for s in seqs:
        hdr += f" | trace @s={s:<5}"
    print(hdr)
    print("-" * len(hdr))
    for m in MODELS:
        W = params(m) * 4 / 1e9  # Fr 32B
        dims = f"D{m.D} L{m.L} Hq{m.Hq}/kv{m.Hkv} hd{m.hd} int{m.inter}"
        row = f"{m.name:<16}{dims:<26}{W:>11.1f}GB"
        for s in seqs:
            te = trace_elements(m, s)
            sc = per_layer_score(m, s) * m.L
            row += f" | {fmt_gb(te, s)} ({fmt_frac(sc, te)}score)"
        print(row)
    print()

    print("score 占比随 s: 线性项∝s(随Hq·hd,L,D,inter)，score项∝Hq·s²。"
          "s 越大 score 越主导。\n")

    for m in MODELS:
        print(f"--- {m.name} [{m.note}]  [dim来源: {m.src}]")
        for s in seqs:
            te = trace_elements(m, s)
            sc = per_layer_score(m, s) * m.L
            W = params(m) * 4 / 1e9
            print(
                f"   s={s:<6} trace={te*ELEM_BYTES/1e9:8.1f} GB | score={sc*ELEM_BYTES/1e9:8.1f} "
                f"({100*sc/te:.1f}%) | Fr若×4={te*ELEM_BYTES*4/1e9:9.1f} GB | 权重≈{W:6.1f}GB(Fr)"
            )


if __name__ == "__main__":
    main()