#!/usr/bin/env python3
"""Convert Llama-2-13b-hf PyTorch .bin shards to a single safetensors file.

Usage: python3 convert_13b_to_safetensors.py
Output: model_cache/meta-llama/Llama-2-13b-hf/model-merged.safetensors (~26GB BF16)
"""
import json
import os
from pathlib import Path

import torch
from safetensors.torch import save_file

SRC = Path("/home/lww/project/06_zkp/deep-prove/zkml/model_cache/meta-llama/Llama-2-13b-hf")
OUT = SRC / "model-merged.safetensors"

shards = sorted(SRC.glob("pytorch_model-*.bin"))
assert shards, f"no .bin shards in {SRC}"
print(f"loading {len(shards)} shards...")

state = {}
for shard in shards:
    print(f"  {shard.name}")
    state.update(torch.load(shard, map_location="cpu", weights_only=True))

# drop duplicated rotary embeddings if present (derived at runtime)
state = {k: v.contiguous() for k, v in state.items() if "inv_freq" not in k}
print(f"total tensors: {len(state)}")

save_file(state, str(OUT), metadata={"format": "pt"})
print(f"written: {OUT} ({OUT.stat().st_size / 1e9:.1f} GB)")
