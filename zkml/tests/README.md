# 流式证明可复测脚本套件——执行文档

> 适用基线：`feat/streaming-trace-prove` 分支，含 P2 修复（rayon i64 matmul，commit `7e308bc1`+）。
> 测试对象：`--streaming` 流式分段证明管线（prompt 模式，单次前向 + 分段证明 + 合并 + verify）。

---

## 1. 前置条件

| 项 | 要求 | 检查方式 |
| --- | --- | --- |
| 二进制 | `cargo build --release -p zkml --bin bench-llm`（脚本缺失时自动构建） | `ls target/release/bench-llm` |
| 权重 | model_cache 下各模型 `model.safetensors` >1MB 且非 LFS 指针 | 脚本自动预检，失败给修复指引 |
| tokenizer | 各模型目录 `tokenizer.json` 非空 | 脚本自动预检 |
| 内存 | ≥350GB（llama s≥1024 需 ≥550GB） | 脚本自动告警 |
| 磁盘 | /home ≥50GB（store 页在 /tmp，占 / 分区） | `df -h /home` |

权重缺失时补拉（gemma-1b 为 gated 需 HF_TOKEN）：

```bash
cd zkml/model_cache
HF_ENDPOINT=https://hf-mirror.com ./download_missing_weights.sh          # 全部
HF_ENDPOINT=https://hf-mirror.com ./download_missing_weights.sh g1b tiny # 指定项
```

## 2. 快速开始

```bash
cd zkml/tests

# 单个测试
./run_test.sh gpt2_s1024              # 冒烟/最大可测长度
./run_test.sh gemma270m_s4096         # 4k 档
nohup ./run_test.sh llama7b_s1024 > llama1024.log 2>&1 &   # 长测试后台跑

# 全量战役（顺序执行 ~20-26h，跨天；单失败不中断）
nohup ./run_all.sh > campaign.log 2>&1 &
tail -f campaign.log

# 只跑一个模型的全部档位
nohup ./run_all.sh 'gemma270m_*' > c_gemma.log 2>&1 &
nohup ./run_all.sh 'llama*'      > c_llama.log 2>&1 &
```

## 3. 测试矩阵（14 个测试点）

| test_id | 模型 | 序列 | 预期 | 预估时长 | 内存峰值预估 |
| --- | --- | ---: | --- | ---: | ---: |
| gpt2_s128 | gpt2 124M | 128 | ✅ | 10 min | 30 GB |
| gpt2_s1024 | gpt2 | **1024（最大可测）** | ✅ | 25 min | 84 GB |
| gpt2_s4096 | gpt2 | 4096 | ⛔ **预期架构拒绝**（位置嵌入上限 1024） | 1 min | — |
| gemma270m_s512 | gemma-270m (SWA512) | 512 | ✅ | 5 min | 56 GB |
| gemma270m_s1024 | | 1024 | ✅（SWA 生效） | 12 min | ~60 GB |
| gemma270m_s4096 | | 4096 | ✅（P2 修复后可行） | ~1 h | ~150 GB |
| gemma270m_s8192 | | 8192 | ✅ | ~2 h | ~250 GB |
| gemma270m_s16384 | | **16384（最长档）** | ✅ | ~3-4 h | ~400 GB |
| gemma1b_s512 | gemma-3-1b (SWA512) | 512 | ✅ | 40-70 min | ~60 GB |
| gemma1b_s1024 | | 1024 | ✅ | ~1 h | ~90 GB |
| gemma1b_s4096 | | 4096 | ✅ | ~2-3 h | ~250 GB |
| gemma1b_s8192 | | 8192 | ✅ | ~4 h | ~330 GB |
| gemma1b_s16384 | | 16384 | ✅ | ~7 h | ~420 GB |
| llama7b_s512 | llama-2-7b | 512 | ✅ 基线 1.88h | 1.9 h | 420 GB |
| llama7b_s1024 | | **1024（实际最大可行）** | ✅ | 4-7 h | ~500 GB |
| llama7b_s4096 | | 4096 | ⛔ 预估数天（T_pad 1.9TB Fr）——默认不跑 | 数天 | — |
| llama32_s512 | Llama-3.2-1B | 512 | ⚠️ 实验性（llama3 rope-scaling 支持性待验证） | ~1 h | ~150 GB |

> 注：时长为同机器（128 核 CPU / 1TB RAM，无 GPU）经验值，供排期参考。

## 4. 结果与留痕

每次运行独立目录（时间戳命名，互不覆盖）：

```
zkml/model_cache/results/<RUN_ID>/
├── INDEX.txt                  # 战役索引：每测试 exit/verdict/失败摘录（run_all.sh 生成）
├── <test_id>.log              # RUST_LOG=info 完整日志
├── bench.<test_id>.*.csv      # 全量指标
└── summary.<test_id>.txt      # 自动提取的分析汇总（复盘入口）
```

`summary.<test_id>.txt` 包含（复盘分析所需全部数据）：

- **分段数**（splitting the model in N chunks）
- **run loop 分项打点**：`forward` / `complete_dispatch` / `pin_release`（秒）
- **finish 打点**：证明排空 + 合并 + verifier IO
- **verify 逐段日志**（首尾各 2 行 + 失败时的精确段号）
- **store 流量**：fetch / disk_reads（流式路径断言 **disk_reads=0**）
- **内存峰值**、proof 大小、git commit、二进制时间

## 5. 判定规则

| verdict | 含义 |
| --- | --- |
| PASS | exit=0 且（若到达 verify）逐段验证通过 |
| PASS-expected-failure | 预期失败的测试点（gpt2_s4096）确实失败 |
| FAIL | 非预期退出/verify 失败——**需要分析**（把 summary 给我，或对照 §6 排查） |
| UNEXPECTED-SUCCESS | 预期失败却成功——同样需要分析 |

## 6. 常见问题排查

| 症状 | 原因与处理 |
| --- | --- |
| `is a git-lfs pointer file` | 权重未真正下载：`download_missing_weights.sh` 补拉 |
| `tokenizer.json 缺失（0 字节）` | 从镜像重拉：`curl -L -o <路径>/tokenizer.json https://hf-mirror.com/<repo>/resolve/main/tokenizer.json` |
| `No space left on device` | /home 满：删 `model_cache/*.pk`（params 可重新生成，llama-7b 的 pk 达 165GB） |
| OOM killed | 内存不足：降低并发（本套件本身串行）、换更小序列；llama s≥1024 需独占机器 |
| `Truncating to N while first dimension is M` | 模型架构上限（如 gpt2 位置嵌入 1024）——预期行为 |
| verify 报 `0th round's prover message not consistent` | **P1 偶发失败**（可靠性缺陷非安全缺陷）：把 summary 交回分析，勿删日志 |
| `has overflowed its stack` | 证明路径存在深层递归，在 ~2MB 栈的 rayon worker / 测试线程上溢出。bench-llm 已内置全局池 64MB 栈；跑 `cargo test` 需 `RUST_MIN_STACK=67108864 cargo test ...`（长序列测试尾段 OOM 见 M 文档 §7：logits MLE 物化 8k 即触顶） |

## 7. 复盘对照基线（P2 修复前 → 后）

| 项 | 修复前 | 修复后 |
| --- | --- | --- |
| llama-7b s=512 端到端 | 14.15h（125 段） | **1.88h（48 段）** |
| gemma s=1024 前向 | 单线程（57min @270m） | rayon 并行（分钟级） |
| 磁盘读 | — | 全程 0（不变量） |

完整历史数据见 `docs/zkml_bootcamp/M_多模型多序列测试报告.md`。
