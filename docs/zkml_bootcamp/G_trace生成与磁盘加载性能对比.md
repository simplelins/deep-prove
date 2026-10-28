# G · Trace 的存储机制与「磁盘加载 vs 直接生成」性能对比（实测）

> **定位**：回答两个问题——① bench-llm 生成 trace 时数据是怎么落盘、又是怎么读回来的（存储/读取机制全走读）；② 把 trace 从磁盘加载回来，与重新生成一次 trace，性能差多少（给出实测数据而非感觉）。
> **读者**：关心 bench-llm 耗时构成、以及"生成一次、反复证明"是否可行的工程师。
> **依据**：本文全部周转代码行号来自当前仓库实际走读；性能数据来自 2026-10-22 三次实机跑分（GPT-2 124M，128 核 CPU，release 编译，NVMe 盘）。benchmark 代码已入库，可复现（§3）。

---

## 0. 结论先行

**把 trace 从磁盘加载回来（全部 592 个 handle、约 0.7 GiB i64 逻辑数据），不到 1.5 秒；在同一输入下重新生成一次同样的 trace（精确口径：只在"产出 trace 的那次完整前向"计时），要 1~2 分钟。磁盘加载 ≈ 生成耗时的 1.1~1.5%，快约 70~92 倍。** 且该比例在数据量翻倍（ctx 64→128）时基本不变，可外推到更大 ctx。

> **计时口径说明**：bench 的 `inference_time` 包住的是 `run_elements` 全量（逐 token 造句 + 最后一轮全图前向）。本测试将两边拆开：**句子逐 token 推理几乎与最后一次全图前向耗时相同**（各占一半，124.4s vs 60.6s），因此"生成 trace"的公平口径 = 精确的完整前向计时。
>
> **GPU 后端（cuda）实测见 §4.4 / §4.5**：ctx=128 精确生成降到 **1.0~1.4s（≈88×）**，加载优势消失（与生成同价），且新增一次性固定成本 NVRTC JIT + i64 kernel autotune（每次新进程最长约 1 分钟）；**ctx=1024 反转折返**——生成 16.4s，而 store 重载要 25.8s（157%）、单文件 26.6s（162%）：加载吞吐恒定 ~470 MB/s（与后端无关），随数据量线性变贵，GPU 后端大 ctx 下重新生成反而更快。

| 对比 | ctx=64 | ctx=128 |
| --- | ---: | ---: |
| 直接生成 trace（精确口径，冷） | **60.6 s** | **123.2 s** |
| （参考）`run_elements` 全量含造句 | 124.4 s | 247.9 s |
| 从 GenStore 加载（592 handles 全量重载） | **0.66 s** | **1.42 s** |
| 整 trace 单文件序列化（写） | 0.73 s / 138.7 MiB | 1.53 s / 302.4 MiB |
| 单文件读回+反序列化 | **0.87 s** | **1.79 s** |
| 加载(store) / 生成 | 1.1%（≈92×） | 1.2%（≈87×） |
| 加载(文件) / 生成 | 1.4%（≈70×） | 1.5%（≈69×） |

三个关键发现：

1. **生成的耗时 100% 由推理计算主导，且逐 token 造句循环与最终全图前向几乎等价**（run_elements 总 124.4s，其中产出 trace 的最后一次前向 60.6s）。落盘（msgpack 编码 + 文件写）在 `StoreRunner` 的后台线程里与推理流水线化，对生成总时长的贡献不可见。
2. **磁盘加载的瓶颈不是读盘，而是 CPU 解码 + burn tensor 重建**（~470 MB/s 逻辑体积）。把 store 的 LRU 缓存从 200 MiB 砍到 2 MiB、强制每一次 fetch 都走磁盘文件，重载耗时几乎不变（0.66s → 0.68s）：69 MiB 的顺序盘读只值几十 ms。
3. **当前"落盘"是进程内暂存，不是持久化**：store 文件虽写在磁盘上，但跨进程**无法被发现**（§2.3）。如果真要"生成一次、换进程反复加载"，需要单文件方案（本测试已验证可行且快）或改 `GenStore`（§5）。

---

## 1. 背景：trace 在哪儿生成、在哪儿消费

- `zkml/src/bin/bench/llm.rs:320-341`：推理计时段内，分别走
  - 分布式：`InferenceEngine::LLM(driver).run(vec![input_tensor], &mut GenStore::default(), &split_node_info)`
  - 单机：`driver.run_elements(input_tensor, &mut GenStore::default())`
  两者都用 `GenStore::default()` 作为存储，随后把返回的 `Trace` 直接喂给 `driver.prove(...)` / `distributed::run_distributed(...)`。
- 所以 bench 里**"生成 trace"这一步天然包含边算边写盘**：理解 gen 与 load 的真实开销，必须先把写/读机制吃透。

---

## 2. 存储与读取机制全走读

### 2.1 生成路径：每层一帧，后台线程写盘

调用链（单机路径）：

```
run_elements (zkml/src/model/llm.rs:380)
  → run_elements_with_split_info (llm.rs:360)   runner = BaseRunner::from(store.clone())
    → run_elements_with_runner (llm.rs:417)
        逐 token 迭代 (llm.rs:495): for unpadded_seq_len in num_input_tokens..=max_window
            self.model.run_with_runner(&mut runner, input_handles)
        最后一轮 (llm.rs:568-572): run_with_split_nodes_info(vec![full_sentence], store, split_node_info)
```

- 运行器是多层洋葱：`HandleLifetimeRunner(TraceRunner(StoreRunner(BaseRunner)))`（`zkml/src/model/mod.rs:1587-1616`）。
- **`StoreRunner`（`mod.rs:481`）是写入的核心**：wrap 一个 `mpsc` channel + 一个后台线程。每层跑完后，`run_layer`（`mod.rs:600-664`）把该层的输入 handle、输出 handle、证明用 Element handle（含 split 后各 chunk 的 proving handle）逐个 `send()` 给后台线程。
- 后台线程（`mod.rs:496-527`）逐帧执行 `store.store(&storage_key, &data)`，即 `impl GenericStore for GenStore` 的 `store`（`tenstore/src/genstore.rs:223`）。

写入落地（`tenstore/src/local.rs:130` `DiskStore::store`）：

1. `rmp_serde::to_vec(&data)` — msgpack 序列化；
2. 文件名 = `DefaultHasher(kind_len + kind + id_len + id)` 的 u64 十进制字符串（**不含 run_id**），直接放 `TempDir` 根目录（`GenStore::default()` = `new_temporary(200MiB LRU)`，`genstore.rs:190-195`）；
3. `BufWriter + File::create` 覆盖写文件；
4. 同步把序列化字节放入 **LRU 内存缓存**（`weight_lru::LruCache`，按 `data.len()` 计权重，超 200MiB 淘汰）。

两个重要细节：

- **写放大**：LLM 逐 token 迭代复用同一套 node storage key（`mod.rs:1094-1097` 注释说明 remix/run 复用同一 key），每个 key 在每次迭代都被全量覆盖重写。真实 bench 场景（`--min_user_len 1 --max_context 1280`）意味着**每次 token 迭代都重写整份 trace 体积**——盘读盘写、CPU 编码量 ≈ trace 体积 × ctx，只是都被后台线程藏掉了。
- **trace 自己还留有一份内存拷贝**：`TraceRunner` 里的 step 手柄是 `handle.isolate()` 的独立副本（`mod.rs:431-445`），`HandleLifetimeRunner` 的引用计数 `dry()` 擦不掉它们。所以**单机（非分布式）路径下，prove 消费 trace 时数据其实还在内存里**，store 只对 dry 过的 handle 才是数据源。

### 2.2 读取路径：LRU 命中 or 整文件读回 + 解码重建

任何代码访问 dried handle（`wrapped_tensor()` / `tensor()`，`zkml/src/tensor/handle.rs:420,98`）都触发 `load()` → `store.fetch(&storage_key)`：

1. `GenStore::fetch`（`genstore.rs:211-221`）按 kind 分派到 Local/Tmp/Remote；
2. `DiskStore::fetch`（`local.rs:117-127`）：
   - `fetch_bytes_internal`（`local.rs:189-213`）先查 `storage: HashMap<Uuid, HashMap<InternalKey, Storage>>` 索引拿到文件路径，再 `cache.try_get_or_insert`：LRU 命中直接返回缓存字节；miss 时 `BufReader::read_to_end` **整文件一次性读入**；
   - `rmp_serde::from_slice` 解码成 `Vec<T>`，随后 `Tensor::new_with_unpadded_shape(...)` 在 burn 后端里重建张量。

### 2.3 机制上的三个"坑"（决定持久化可行性）

1. **run_id 随机**：每个 `GenStore` 实例生成随机 `run_id: Uuid`（`genstore.rs:63-67`），索引键是 `(run_id, id, kind)`，而文件名只含 `(kind, id)` 的 hash。**进程重启后 `storage` 内存索引为空**，磁盘上的文件虽在却永远无法被发现（`fetch` 直接返回 `KeyUnknown`）。
2. **`GenStore` 在 serde 里被整体跳过**：`TensorHandle` 的 `store` 字段是 `#[serde(skip)]`（`handle.rs:27,52`）。整条 `Trace`/`Step`/`TensorHandle` 链都实现了 Serialize/Deserialize（所以"把 trace 序列化到单文件"是现成可行的），但反序列化回来的 handle 附的是一个**全新的默认 store**（`GenStore::default()`→`new_temporary`，又是新 TempDir + 新 run_id），之前 store 里的数据接不上。唯一现实的接法是"数据内嵌在 trace 文件里"。
3. **TempDir 生命周期**：`GenStoreKind::Tmp` 持有 `tempfile::TempDir`，store 一被 drop 文件即被删（`genstore.rs:39-43` 注释）。

> 结论：*当前*设计里磁盘 store 是"单进程内跨 runners/线程的暂存层"，不是"跨运行的持久层"。分布式模式（`distributed::run_distributed` + `iop/prover_graph.rs:178-181` 的 `dry_trace` → chunk prover 各自从 store 重新拉取）正是利用这个机制来降内存，本测试的加载数据因此也适用于分布式 proving 的 trace 拉取开销估算。

---

## 3. Benchmark 设计与复现

新增测试：`zkml/src/model/llm.rs`（test 模块）`bench_trace_disk_load_vs_regen`，默认 `#[ignore]`。

**流程**（用 openai-community/gpt2 safetensors，量化后 261 层 / 259 trace steps，prompt=`ctx-2` 个随机 token 以避开 EOS）。**计时口径**：生成时间只包住"产出 trace 的那一次完整前向"，不包括句子逐 token 推理，也不包括模型加载/量化等初始化：

1. **参考阶段（不计入生成）**：`run_elements` 从随机 prompt 逐 token 生成句子（纯推理），最后产出一次 trace —— 仅用其结果拿到完整输入 token 序列，并打印总耗时作参照；
2. **① trace 生成（精确计时）**：用上一步得到的全序列输入，**从干净模型状态**执行 `driver.model.run_with_split_nodes_info(vec![完整序列], &mut store, None)` —— 即 `run_elements` 内部产出 trace 的最后一步（`llm.rs:568-572`）；计时只围住这一次完整前向 + `StoreRunner` 并行落盘；
3. **② store 加载**：`trace.dry_handles()` 擦掉全部内存副本，遍历 `trace.inputs/outputs/steps` 全部 592 个 handle 逐个 `wrapped_tensor()/tensor()` 强制重载计时；
4. **③ 单文件方案**：`rmp_serde::to_vec(&trace)`（数据内嵌）写盘计时，再读回+反序列化计时；
5. **⑤ 再生成一次**（同进程、相同输入）复测生成时间。

> 实现注意：全序列输入张量要与 `run_elements` 内部一致直接 `Tensor::new` 构造（`llm.rs:556`）；不能走 `tokens_to_tensor`——driver 以 `Some(ctx)` 加载后 `md.context_length == ctx`，其 `len < context_length - 1` 断言会拦下完整长度序列。

复现命令：

```bash
# 默认 ctx=64, LRU=200MiB
cargo test --release -p zkml bench_trace_disk_load -- --ignored --nocapture

# 规模放大
TRACE_BENCH_CTX=128 cargo test --release -p zkml bench_trace_disk_load -- --ignored --nocapture

# 小 LRU：强制每次 fetch 都读磁盘文件（绕过缓存命中）
TRACE_BENCH_LRU_MB=2 cargo test --release -p zkml bench_trace_disk_load -- --ignored --nocapture
```

> 环境修复记录：`zkml/model_cache/openai-community/gpt2/` 下原是 Git-LFS 指针文件（128B），导致 config.json 解析失败；已从仓库根 `model_cache/` 拷入真实 blob 修复（仅本地缓存，与代码无关）。

---

## 4. 实测数据与分析

### 4.1 原始数据（release，GPT-2 124M，128 核；生成 = 精确完整前向口径）

| 指标 | ctx=64 (LRU=200MiB) | ctx=64 (LRU=2MiB) | ctx=128 (LRU=200MiB) |
| --- | ---: | ---: | ---: |
| ① trace 生成（精确口径，冷） | 60.64 s | 61.15 s | 123.19 s |
| ① trace 生成（同进程复测） | 60.51 s | 59.60 s | 116.50 s |
| （参考）`run_elements` 全量（造句+trace） | 124.38 s | 123.28 s | 247.87 s |
| ② store 全量重载 | 0.659 s | 0.675 s | 1.424 s |
| ②b 二次重载 | 0.639 s | 0.655 s | 1.379 s |
| ③ 序列化到单文件 | 0.732 s | 0.729 s | 1.531 s |
| ③ 文件体积 | 138.7 MiB | 138.6 MiB | 302.4 MiB |
| ④ 文件读回 | 81 ms | 76 ms | 165 ms |
| ④ 反序列化(含重建) | 0.866 s | 0.860 s | 1.793 s |
| trace 步数/handles/tokens | 259/592/64 | 同左 | 259/592/128 |
| trace 逻辑体积(i64) | 322.8 MiB | 同左 | 699.7 MiB |
| store 页数 / 落盘体积 | 347 / 68.8 MiB | 同左 | 347 / 150.2 MiB |
| **加载(store)/生成** | **1.1%（≈92×）** | **1.1%（≈91×）** | **1.2%（≈87×）** |
| 加载(文件)/生成 | 1.4%（≈70×） | 1.4%（≈71×） | 1.5%（≈69×） |

（注：ctx=64 生成时间三次独立跑均在 59.6~61.2s，方差 <2%，非常稳定。）

### 4.2 分析

- **生成时间几乎 100% 在推理计算，且逐 token 造句循环与最终全图前向几乎等价**：`run_elements` 总量 ≈ 2× 精确生成时间（124.4 vs 60.6；247.9 vs 123.2）——逐 token 循环的每次迭代也是一次全图前向（`llm.rs:510`），开销与最终产 trace 的 heavy run 同量级。bench 里 `HEADER_INFERENCE_TIME` 包住这两部分；单独核算"生成 trace"成本时应以精确口径为准。ctx=64→128 数据量 ×2，生成 60.6s→123.2s（×2.03，线性）。
- **加载成本结构**：~0.66s 里几乎全是 `rmp_serde` 解码 + burn CPU 后端张量重建（592 个张量、322.8 MiB i64 → ~470 MB/s 逻辑值；按 msgpack 落盘 68.8MiB 折算 ~106 MB/s 线速）。LRU 200MiB vs 2MiB（强制全走盘文件读取）结果一致 → **读盘不是瓶颈**，也无须为加速加载放大 LRU；trace 体积接近/超过 LRU 上限（ctx=128 已到 150.2MiB）时同样不受影响（1.42s）。
- **体积换算**：store 落盘体积（68.8/150.2 MiB）远小于逻辑 i64 体积（322.8/699.7 MiB）——**msgpack 对小量化值用变长整数编码（1-2 B/值）**，实测压缩比 ~4.7:1。单文件方案体积（138.7/302.4 MiB）大于 store，是因为同一个 storage key 被 input/output 两路引用各自内嵌了一份。
- **写放大的说明**：精确生成的 61s 内，`StoreRunner` 只把全部 trace 数据完整写盘一次（后台并行）；而 `run_elements` 的逐 token 循环里同一批 key 被每个迭代覆盖重写（ctx=64 是 3 轮迭代 ×3 倍物理写量）。`--max_context` 配 `--min_user_len=1` 的真实 bench（逐 token 全程）写放大 ≈ ctx 倍。

### 4.3 热点归属：为什么 CPU 版生成这么慢（perf 实测归因）

对"精确生成窗口"做 `perf record -F 199 -g`（ctx=128 全测试 499.5s，108,417 样本，Total Lost 0）：

| 符号（demangle 失败已折叠） | Children | Self | 归属 |
| --- | ---: | ---: | --- |
| `burn_ndarray::ops::matmul::<FnMut>::call_mut` | **89.74%** | 89.32% | ndarray 通用标量 GEMM（i64 整数乘） |
| `__memmove_avx512_unaligned_erms` | 1.22% | 0.75% | 张量拷贝（burn clone / StoreRunner get_data / isolate） |
| `asm_exc_page_fault` (kernel) | 1.10% | 0.01% | 热分配的缺页 |

**核心结论：trace 生成 ~90% 的 CPU 周期全部花在 burn-ndarray 后端的整数矩阵乘上，且是单线程执行。**

证据链：

1. **单线程实锤**：总事件计数 1.333e12 cycles / 499.5s ≈ 2.67e9 cycles/s ≈ **1 颗 2.7GHz 的核跑满**；108,417 样本 / (199 Hz × 499.5 s) ≈ **1.09 个活跃核**。128 核机器上 rayon 线程池（128 线程）几乎全程闲置。
2. **为什么没并行**：burn-ndarray 的 matmul 只在 **batch 维** 用 rayon（`iter_range_par!(0, num_out_batches)`，`burn-ndarray/src/ops/matmul.rs:26-53`）；LLM 层的 matmul 全是 2D（batch=1）→ 无批可分 → 内层 `ndarray::linalg::general_mat_mul(E=i64)` 串行。而 `general_mat_mul` 只有 **f32/f64** 才走 matrixmultiply（其 SIMD kernel / threading≤4 线程）；**i64 走 ndarray 自带的通用标量 kernel**（无 SIMD、单线程）。
3. **合理性核算**：seq=128 一次可证前向约 2×124M×128 ≈ 3.2e10 次 i64 MAC；生成窗口内 ≈ 2.67e9×119.6 ≈ 3.2e11 cycles → **每个 MAC ~10 cycles**。对"标量乘-加 + 载入 + zip 累加、无 SIMD 无并行"的循环来说这是预期均值——**不是 bug，是后端选型的固有成本**（CPU 默认特性 = `NdArray`，zkml/Cargo.toml:28；f32/f64 之外的整数类型只有标量路径）。
4. **run_elements 为何 ≈ 2× 精确生成时间**：perf/log 显示"造句"第一次全量迭代 [126] 耗时 120.1s、KV-cache 增量步（[1] 个 token）仅 ~2s，而 trace 的 heavy run 又是从头跑 119s——可证 trace 需要一次干净的完整前向，所以同一段计算做了两遍（设计如此）。
5. **写盘/编码不在热点**：rmp encode、文件写、StoreRunner 线程都没出现在热点前列——呼应 §2.1"落盘被流水线化"。

**优化杠杆（按预期收益排序）**：

| 方案 | 改动 | 预期 |
| --- | --- | --- |
| A. 行维切块喂给 batch 并行 | 全层算子的 lhs `[M,K]`→`[B,M/B,K]`（rhs `[1,K,N]` 广播），让 `iter_range_par` 分派 rayon | 单次前向可吃满多核，gen 理想 10~30×（上限受内存带宽/小块效率限制） |
| B. GPU 后端 | bench 开 `--features cuda`（或 wgpu）；`Backend` 类型切 `Cuda`（zkml/src/backend/mod.rs） | gen 与 prove 同走 GPU kernel（zkml 已有 gpu kernels 模块；需验证 i64 依赖的完整性） |
| C. 降整数位宽 + SIMD | burn-ndarray 开 `simd`(macerator)：i32 走 `_mm_mullo_epi32` 4 向 | 需把可证量化位宽压到 i32，前提工程量较大；i64 的 SIMD 乘支持本身缺失（macerator 中 i64 仅有 add/sub） |
| D. 共享两次前向 | 让 trace run 复用造句阶段的 KV/中间态 | 省 ~50% 的 run_elements 总时长，但 trace 可证性要求完整重放，收益受设计限制 |

> 备注：`perf report --time` 切片两份报告内容相同（对称阶段算力同分布，89.7% matmul 的归属在任一窗口内均成立）。要复验切片可用 `--time 243.9,363.5`（对应 perf_run.log 里 trace-gen 窗口）。

### 4.4 GPU 后端补充实测（cuda feature，2026-10-23）

同一 bench 以 `zkml/runlog/trace-perf.sh` 包裹、`--features cuda` 跑了一版（ctx=128，cubecl_cuda，同 GPT-2 124M；autotune 缓存部分命中：`Loaded 23 autotune cached entries`）：

| 阶段 | CPU (ndarray, i64 标量) | GPU (`cuda` feature) | 备注 |
| --- | ---: | ---: | --- |
| load+quantize（含 f32 autotune） | ~9 s | ~50 s | GPU 侧多出的 ~40s 是 f32 kernel 的 NVRTC JIT+调优（一次性） |
| run_elements 全量（ref） | 243.8 s | 81.2 s | GPU 版大头**不是算力**：首次迭代 61.6s 里几乎全是 i64 各 shape 的 matmul/reduce autotune（04:16:36→04:17:38 连续 Tuning），heavy run 15s 里也还有 ~10s 在补 tune |
| 精确生成 trace（cold） | 123.2 s | **1.3998 s** | **≈88× 提速**（= §4.3 方案 B 实测落地）；与 warm 差 ~0.4s = 残余一次性 JIT |
| 精确生成 trace（warm） | — | **1.0077 s** | |
| 从 GenStore 加载 | 1.42 s | 1.378 s | 持平——重载成本在 CPU 侧 msgpack+重建，与后端无关 |
| 整 trace 单文件写 / 读回 | 1.53 / 1.79 s | 1.412 / 1.79 s | 数据体积一致（150.3 MiB pages / 302.6 MiB 单文件）→ 落盘格式与后端解耦 |

perf（32,184 样本，386.6e9 cycles；报告依旧未按 gen 窗口切片成功，但结论清晰）：

- **CPU 周期的大头全在 CUDA JIT**：`nvrtcCompileProgram` 55%（children）、`cuModuleLoadData` ~23%、`__cuda_CallJitEntryPoint` ~22%——即 cubecl 首次运行为每种 kernel×shape×dtype 编译 PTX。真正的 GPU 算力不体现在 CPU cycles 上（perf 只测 CPU；测 GPU 利用率需 nsys/ncu）。
- 剩下可见的 CPU 项：crossbeam epoch（StoreRunner 通道）~3%、`__memmove` ~1%、`rmp_serde` encode+decode ~2%、`_IO_getc` ~1.3%——与 CPU 版一致，证明**存储链路本身依然不是瓶颈**。平均 CPU 占用 ≈ 2.39e9 cycles/s（GPU 计算期间 CPU 近乎空闲）。

**GPU 版改变了三个结论的权重：**

1. **"重生成昂贵"消失**：生成 1.0~1.4s 后，从 store 重载（1.378s）与单文件读回（1.79s）已**持平甚至更慢**（98%→128%）。磁盘加载从"70~90× 加速"退化为"与生成同价"，此时落盘的价值回归到**跨进程持久化与分布式拉取**（§5），而非速度。
2. **新的固定成本是 JIT/autotune**：每个新 (kernel 变体 × matmul shape × dtype) 组合都要 NVRTC 编译+基准调优，i64 的 shape 变体（General/VecMat/Fused，m=1 与 m=128 又不同）首次启动要 ~1 分钟。这是进程级一次性成本，但**每次新进程都可能重跑**（tune cache 命中不完整，`N9: Loaded 0 autotune cached entries`）——值得优化：固化 tuner 决策、预热编译、或持久化 cubin。
3. **瓶颈转移链完整**：CPU 版 90% 在单线程 i64 标量 GEMM（§4.3）、GPU 版计算本体几乎免费，固定成本变成编译调优——优化杠杆表中的方案 B（GPU 后端）已实测，收益 ≈88×，与 §4.3 单线程根因互相印证。

### 4.5 GPU 后端 ctx=1024 复测：存储链路首次反超生成（2026-10-23）

同一 GPU bench 改 ctx=1024（perf_run1024.log，全程 370.5s；上一轮 ctx=128 的 tune 缓存部分生效，Tuning 次数从 43 降到 32，新增的 m=1024 系列 shape 仍需现场 JIT）：

| 阶段 | ctx=128 | ctx=1024 | 实测倍数 | （线性基准 = ×8） |
| --- | ---: | ---: | ---: | --- |
| 精确生成 trace（cold） | 1.3998 s | 16.443 s | **11.7×** | 超线性（attention 二次项） |
| 精确生成 trace（warm） | 1.0077 s | 15.869 s | 15.8× | 同上 |
| 从 GenStore 加载（dry+hydrate） | 1.378 s | **25.840 s** | 18.8× | |
| 单文件序列化（写） | 1.412 s | 19.840 s | 14.1× | |
| 单文件读回 | 0.178 s | 2.830 s | 15.9× | |
| 单文件反序列化 | 1.792 s | **26.610 s** | 14.9× | |
| store 落盘体积 | 150.3 MiB | 2570.5 MiB | 17.1× | |
| 逻辑数据体积 | 699.7 MiB | 11645.3 MiB | 16.6× | |
| 单文件体积 | 302.6 MiB | 5156.8 MiB | 17.0× | |

**headline：`=== summary` 变号**——reload 25.84s = 生成的 **157%**，单文件 161%：GPU 后端大 ctx 下**重新生成比读盘更快**（CPU 后端则不会：123.2s@128 外推 ~24min@1024，重载仍占优 ~55×）。

机理三条：

1. **加载吞吐是设计常数**：11,645 MiB / 25.84s = **469 MB/s** 逻辑数据，与 CPU 版 ctx=128 的 493 MB/s 几乎相同——hydrate 从不因后端/规模变快，只随数据线性变贵；gen 却吃到 GPU 提速 88×。两者的斜率差就是反超点。
2. **数据量超线性**：8× token → 体积 ~17×：attention 中间张量按 ctx² 增长，1024 时已占主导。store LRU(200 MiB) < pages(2570.5 MiB) → 全走盘 fetch，但也无碍（盘读 2.8s 只值全程 1%）。
3. **加载链路成本暴露**：perf（118,242 样本，1.206e12 cycles，~1.6 活跃核）：
   - msgpack 编解码 ~15%（`rmp_serde` serialize_i64 4.9% + decode 三项 ~10.5%）：5.4 GiB 逐值 visitor 编解码，单线程；
   - `__memmove` 10.5% + 缺页 ~10.4%（fresh alloc 11.6 GiB 的 page fault：`do_anonymous_page` 5.5%、`mem_cgroup_charge` 1.9%）——为"分配+搬运"，**之前 <2% 的存储成本项在此规模下放大一个量级**；
   - JIT 仍是大头（nvrtc 26.7% + cuModuleLoad 11.5% + CallJit 10.4%），绝对量与 ctx=128 相当，占比被总长摊薄；
   - `cuEventSynchronize` 4.1%（gen 主循环逐 op 同步等待 GPU）；文件 read/write syscall 各 ~1.6%——**5.4 GiB 盘 I/O 只占 ~3%，瓶颈确认在 CPU 编解码/内存搬运，不在盘**。

> run_elements 分解：~138s = 1021 个 KV 增量步（≈0.135 s/token，GPU 上很快）+ heavy 前 ~9s 补 tune + heavy 归程 ≈ 精确生成一遍（与 CPU 版"可证前向=再造一遍句子"结论同构）。

**若要 GPU 大 ctx 下也保住"读盘更快"**（按 profile 收益排序）：

| 杠杆 | 说明 | 预估 |
| --- | --- | --- |
| 二进制 raw payload | 变长 msgpack 换定长小端 i64 buffer，visitor 逐值解码 → 大块 memcpy | 编解码 ~15% CPU 全消；26.6s → ~10s 量级 |
| 逐 handle 分片解码 | rmp serde 当前单线程；rayon 按 handle 并行 | 理想 ~核数倍 |
| mmap 零拷贝 + 大块预分配 | 消 memmove 与 fresh alloc 缺页（两者占 ~21%） | 与上两项部分重叠 |
| 解码/上传流水线化 | hydrate 与 GPU 上传重叠 | 受 GIL 单线程限制，交给上面两项 |

### 4.6 对 bench 与工程的意义

- **bench 可跳过重复生成吗？** 可以。若 bench 流程需要多次消费同一 trace（调试 repro、扫 batch/threads 参数），把它存成单文件再读回来，一次省 ~1~2 分钟（ctx 64/128 实测线性外推），比重新生成划算 ~70~90 倍。
- **分布式 proving 的 trace 拉取**：每 chunk 的 prover 从 store 重新拉取（re-hydrate）的开销就是 §4.1 "②" 的量级，可与整个 chunk 证明时间相抵，几乎不构成新的串行瓶颈。
- **外推到生产规模**（s=512 ≈ 数据相对 s=128 再 ×4）：加载预计 ~6s、单文件 ~1.2 GiB、生成进入分钟级——结论依然是"重载可忽略、重生成昂贵"。
- **GPU 后端注意**：§4.4 / §4.5 实测后，"重载 vs 重生成"的速度优势只在 CPU 后端成立；GPU 后端 ctx≤128 两者同价，ctx≥1024 重新生成反而快 ~35%。核心价值回到持久化与分布式阶段的重放一致性。

---

## 5. 若要"跨进程/跨运行"持久化 trace：两条可行路线

**路线 A：整 trace 单文件（当前代码即可用，测试已验证）**

- `Trace`/`Step`/`NodeOut`/`TensorHandle` 全部实现 `Serialize/Deserialize`，store 字段 serde-skip，reload 后数据内嵌无需 store；
- 读回 0.87~1.79s（含读文件），比重新生成快 ~70~92×；
- 代价：文件比 store 大 ~2×（引用重复），且恢复出的 handle 没有可用 store（若后续链路还要写/读 store，需 `attach_store` 一致的新 store，但已有文件数据无法复用）。
- 适合：bench 复现、离线分析、单次生成多次 prove。

**路线 B：GenStore 支持持久化（改动小，收益是保持现有引用机制）**

- 加固定 run_id 的构造器（如 `new_local` 后 `with_run_id(固定名)` 的变体，或构造时显式传入 run id）；
- 持久化一份 `manifest`（run_id + key→文件映射），启动时重建 `storage` 索引；
- 文件名本就不含 run_id，只需索引持久化即可复用旧数据。

---

## 6. 变更清单

| 文件 | 变更 |
| --- | --- |
| `zkml/src/model/llm.rs` | 新增 `#[ignore]` benchmark `bench_trace_disk_load_vs_regen`（含 `TRACE_BENCH_CTX` / `TRACE_BENCH_LRU_MB` 环境变量、store 体积统计、比例汇总） |
| `zkml/model_cache/openai-community/gpt2/` | Git-LFS 指针文件替换为真实 blob（本地缓存修复，非代码变更） |

### 附：机制相关核心代码索引

| 机制 | 位置 |
| --- | --- |
| store 写入入口 | `tenstore/src/genstore.rs:223` (`impl GenericStore for GenStore::store`) |
| 落盘+LRU 双写 | `tenstore/src/local.rs:130-233` (`DiskStore::store*`) |
| 文件名规则(不含 run_id) | `tenstore/src/local.rs:37-47` (`InternalKey::rooted_at`) |
| 读取(LRU→盘) | `tenstore/src/local.rs:117-127, 189-213` |
| 后台写盘线程 | `zkml/src/model/mod.rs:481-534` (`StoreRunner`) |
| 每层发送到 store | `zkml/src/model/mod.rs:600-664` (`StoreRunner::run_layer`) |
| 引用计数→dry | `zkml/src/model/mod.rs:995-1043` (`HandleLifetimeRunner`) |
| dried 后的重载(load) | `zkml/src/tensor/handle.rs:420-446` |
| 逐 token 迭代生成 | `zkml/src/model/llm.rs:495-517` |
| 最终一次全量前向出 trace | `zkml/src/model/llm.rs:568-572`；`zkml/src/model/mod.rs:1587-1625` |
| 分布式的 dry/重载消费 | `zkml/src/iop/prover_graph.rs:178-181` |
