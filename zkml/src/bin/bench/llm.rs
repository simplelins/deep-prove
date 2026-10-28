use anyhow::bail;
use ark_bn254::Bn254;
use chrono::{DateTime, Local};
use clap::{ArgGroup, Parser, ValueEnum, builder::ArgPredicate};
#[cfg(not(feature = "cuda"))]
use dp_crypto::arkyper::HyperKZG;
#[cfg(feature = "cuda")]
use dp_crypto::arkyper::hyperkzg_gpu::HyperKZGGpu;
use libc::{RUSAGE_SELF, getrusage, rusage};
use std::{fs::File, io::Write, path::Path, sync::Mutex, time::SystemTime};
use tenstore::GenStore;
use timed_core::Output;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zkml::{
    ProverContext,
    measure::{self, Measure},
    model::{
        exec_graph::InferenceEngine,
        llm::{Driver, LLMVerifierContext, WithMaxContext},
    },
    parser::{
        file_cache,
        gguf::RawGGUF,
        llm::{
            HFTokenizer,
            models::{gemma3::Gemma3, gpt2::GPT2, llama2::Llama2},
            tokenizer::TokenizerLoader,
        },
        safe::RawSafeTensors,
    },
};

type F = ark_bn254::Fr;
// the hasher type is chosen depending on the feature flag inside the mpcs crate
#[cfg(not(feature = "cuda"))]
type Pcs = HyperKZG<Bn254>;
#[cfg(feature = "cuda")]
type Pcs = HyperKZGGpu<Bn254>;

#[derive(Clone, Debug, ValueEnum)]
#[clap(rename_all = "lower")]
enum Model {
    GPT2,
    Gemma3,
    Llama2,
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Model::GPT2 => write!(f, "gpt2"),
            Model::Gemma3 => write!(f, "gemma3"),
            Model::Llama2 => write!(f, "llama2"),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    group(ArgGroup::new("weights").args(["gguf", "hf"]).required(true)),
    group(ArgGroup::new("length").args(["sequence", "max_context"]).required(true)),
)]
struct LLMArgs {
    /// gguf file to load. It can be a local path or a URL to download.
    #[arg(short, long)]
    gguf: Option<String>,

    /// Hugging Face model ID. It MUST be a safetensors model for now. If the model isn't present
    /// in the cache, it will be downloaded.
    #[arg(long)]
    hf: Option<String>,

    /// max context length (in tokens)
    #[arg(long)]
    max_context: Option<usize>,

    /// When specifying a sequence, the model will try each difference sequence length, just generating 2 tokens each time
    /// So for seqlen = n, it will start with a prompt of n-2 tokens, and generates two tokens.
    #[arg(long, value_delimiter = ',')]
    sequence: Vec<usize>,

    /// min user input length (in tokens)
    #[arg(
        long,
        requires = "max_context",
        conflicts_with = "sequence",
        default_value_t = 1
    )]
    min_user_len: usize,

    /// model to use
    #[arg(short, long, value_enum)]
    model: Model,

    /// DEPRECATED: output file name that records individual methods
    #[arg(short, long, default_value_t = {"bench-llm-deprecated.csv".to_string()})]
    output: String,

    /// Benchmark csv output file name
    #[arg(
        long,
        default_value_if("distributed", ArgPredicate::IsPresent, Some("bench_distributed.csv")),
        default_value_if("measure_layerwise", ArgPredicate::IsPresent, Some("bench_layerwise.csv")),
        default_value = "bench.csv" // Optional: what to use if NOT distributed
    )]
    bench: String,

    /// How many rayon threads to use
    /// If not provided, will use the number of logical cores
    /// If 0, will use the number of physical cores
    #[arg(long)]
    num_threads: Option<usize>,

    /// Profile distributed execution
    #[arg(long, default_value_t = false)]
    distributed: bool,

    /// Use improved accuracy model quantisation and transforms
    #[arg(long, default_value_t = false)]
    accuracy: bool,

    /// basename of the param files to use for the prover and verifier.
    /// e.g. "--params setup" will search for "setup.pk" and "setup.vk" in the current directory.
    #[arg(long)]
    load_params: Option<String>,

    /// Basename of the prover and verifier context files to save.
    /// e.g. "--save_params setup" will save the prover and verifier context to "setup.pk" and "setup.vk" in the current directory.
    #[arg(long)]
    save_params: Option<String>,

    #[arg(long)]
    memory: bool,

    #[arg(long, requires = "distributed")]
    num_chunks: Option<usize>,

    /// Measure layer-wise metrics
    #[arg(long, default_value_t = false)]
    measure_layerwise: bool,
}

const HEADER_MODEL: &str = "model_name";
const HEADER_MODEL_QUANT: &str = "quantization_time";
const HEADER_CONTEXT_GENERATION: &str = "context_generation_time";
const HEADER_MAX_CONTEXT: &str = "max_context";
const HEADER_NUM_THREADS: &str = "num_threads";
const HEADER_MIN_USER_LEN: &str = "min_user_len";
const HEADER_INFERENCE_TIME: &str = "inference_time";
const HEADER_PROOF_SIZE: &str = "proof_size";

/// 生成一次运行的 CSV 输出文件名, 避免多次执行互相覆盖/混淆, 便于横向比较。
/// 规则: `<bench 所在目录>/<bench 主干>.<model>_s<max_context>.<HHMMSS>.csv`
/// - `bench`: 用户 `--bench` 指定的路径(可含目录), 取其主干;
/// - model + s<max_context>: 标识本条数据来自哪个模型/序列长;
/// - HHMMSS: 本次运行时间戳(每次执行不同 → 不覆盖)。
/// 同一进程内 `--sequence a,b,c` 多序列长仍都写入同一时间戳文件(每行带 max_context 列),
/// 跨进程重跑则各得一个独立文件。
fn bench_csv_path(bench: &str, model: &str, max_context: usize) -> String {
    let path = Path::new(bench);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bench".to_string());
    // 获取当前时间戳, 转为日期和时间, 取日期时分秒作为文件名后缀, 避免覆盖
    let now: DateTime<Local> = DateTime::<Local>::from(SystemTime::now());

    // 格式 1：紧凑型，最常用
    let stamp = now.format("%Y%m%d_%H%M%S").to_string();
    let fname = format!("{stem}.{model}_s{max_context}.{stamp}.csv");
    match dir {
        Some(d) => {
            let d = d.join("runlog");
            std::fs::create_dir_all(&d).expect("Failed to create runlog directory");
            d.join(fname).to_string_lossy().into_owned()
        }
        None => fname,
    }
}

fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");
    timed_core::set_output(Output::CSV("bench-llm.csv".to_string()));

    let args = LLMArgs::parse();

    // either its spceified and if 0 it's the physical cores otherwise what is specified but no more than the logical cores
    let num_threads = if let Some(nt) = args.num_threads {
        if nt == 0 {
            num_cpus::get_physical()
        } else {
            nt.min(num_cpus::get())
        }
    } else {
        num_cpus::get()
    };
    info!("Using {} threads", num_threads);
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build_global()
        .unwrap();

    let (max_context, sequence) = if let Some(max_context) = args.max_context {
        (max_context, vec![(args.min_user_len, max_context)])
    } else {
        let mut sequence = args.sequence.clone();
        sequence.sort();
        (
            *sequence.last().unwrap(),
            sequence
                .into_iter()
                .map(|s| (s.saturating_sub(2), s))
                .collect(),
        )
    };

    let bench_file = bench_csv_path(&args.bench, &args.model.to_string(), max_context);
    info!("CSV metrics will be written to `{}`", bench_file);

    info!(
        "Running with max context {} and user_prompt->max_length: {:?}",
        max_context,
        sequence
            .iter()
            .map(|(s, m)| format!("({}->{})", s, m))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let (driver, tokeniser) = parse_model(&args, max_context)?;
    mem_snap("1.load_model");

    let mut premeasure = Measure::new()
        .with(HEADER_MODEL, &args.model.to_string())
        .with(HEADER_NUM_THREADS, &num_threads.to_string());
    if args.measure_layerwise {
        premeasure = premeasure.enable_layerwise_measures();
    }
    info!("Converting model into provable model...");
    let (mut driver, _metadata) = if !args.accuracy {
        premeasure.r(HEADER_MODEL_QUANT, || driver.into_provable_llm(None))?
    } else {
        match args.model {
            Model::GPT2 => premeasure.r(HEADER_MODEL_QUANT, || {
                driver.into_provable_llm_with_transform::<GPT2>(&tokeniser)
            })?,
            Model::Gemma3 => premeasure.r(HEADER_MODEL_QUANT, || {
                driver.into_provable_llm_with_transform::<Gemma3>(&tokeniser)
            })?,
            Model::Llama2 => premeasure.r(HEADER_MODEL_QUANT, || driver.into_provable_llm(None))?,
        }
    };
    mem_snap("2.quantize");

    let (prover_ctx, mut verifier_ctx) = if let Some(ref params) = args.load_params {
        info!("Loading proving contexts from {params}.pk and {params}.vk...");
        let prover_ctx = bincode::serde::decode_from_slice(
            &std::fs::read(format!("{params}.pk"))?,
            bincode::config::standard(),
        )?
        .0;
        let verifier_ctx = bincode::serde::decode_from_slice(
            &std::fs::read(format!("{params}.vk"))?,
            bincode::config::standard(),
        )?
        .0;
        // set to 0 since we're not spending any time generating it
        premeasure.r(HEADER_CONTEXT_GENERATION, || 0);
        (prover_ctx, verifier_ctx)
    } else {
        info!("Generating proving contexts...");
        let (prover_ctx, verifier_ctx): (ProverContext<F, Pcs>, LLMVerifierContext<F, Pcs>) =
            premeasure.r(HEADER_CONTEXT_GENERATION, || driver.context())?;
        (prover_ctx, verifier_ctx)
    };
    mem_snap("3.generate_contexts");

    if let Some(ref save_params) = args.save_params {
        if args.load_params.is_some() {
            bail!("Cannot save parameters if loading parameters is also specified");
        }
        info!("Saving proving contexts to {save_params}.pk and {save_params}.vk...");
        let mut io = File::create(format!("{save_params}.pk"))?;
        io.write_all(&bincode::serde::encode_to_vec(
            &prover_ctx,
            bincode::config::standard(),
        )?)?;
        let mut io = File::create(format!("{save_params}.vk"))?;
        io.write_all(&bincode::serde::encode_to_vec(
            &verifier_ctx,
            bincode::config::standard(),
        )?)?;
    }

    for (user_prompt, max_ctx) in sequence {
        // make a new measure for each trial, but always keep the initial measurements for each sample
        measure::set_global(premeasure.clone());

        measure::set(HEADER_MAX_CONTEXT, max_ctx.to_string());
        measure::set(HEADER_MIN_USER_LEN, user_prompt.to_string());

        driver.with_max_context(max_ctx);
        let user_tokens = driver.random_sequence(user_prompt);
        let input_tensor = driver.tokens_to_tensor(&user_tokens)?;
        let (trace, chunks) = if args.distributed {
            let (chunks, split_node_info) = prover_ctx.split_in_chunks(
                args.num_chunks,
                driver.chunking_strategy(&input_tensor, &prover_ctx)?,
            )?;
            let engine = InferenceEngine::LLM(driver);
            let trace = measure::r(HEADER_INFERENCE_TIME, || {
                info!("Running inference...");
                engine.run(
                    vec![input_tensor],
                    &mut GenStore::default(),
                    &split_node_info,
                )
            })?;
            driver = match engine {
                InferenceEngine::LLM(driver) => driver,
                _ => bail!("Expected LLM engine"),
            };
            (trace, Some(chunks))
        } else {
            (
                measure::r(HEADER_INFERENCE_TIME, || {
                    info!("Running inference...");
                    driver.run_elements(input_tensor, &mut GenStore::default())
                })?,
                None,
            )
        };
        mem_snap("4.trace_gen");

        if args.memory {
            info!(
                "Running memory profiler - will save into {:?}",
                std::env::var("FLAMEGRAPH")
            );
            utils::track::flame_graph_enable();
        }
        let (proof, io) = if args.distributed {
            info!("Running distributed proving locally...");
            distributed::run_distributed(trace, &driver, &prover_ctx, chunks.unwrap())?
        } else {
            mem_snap("5a.prove_before");
            let peak_rss = peak_rss_bytes();
            info!("Running proving locally...");
            let (proof, io) = driver.prove(&prover_ctx, trace)?;
            mem_snap("5b.prove_after");
            let new_peak_rss = peak_rss_bytes();
            if new_peak_rss == peak_rss {
                warn!(
                    "Cannot reliably measure peak memory consumption during proving, setting upper bound"
                );
            }
            // new_peak_rss is the peak memory consumption during proving
            measure::set(
                "prove_full_memory_peak",
                (new_peak_rss / 1024 / 1024).to_string(),
            );

            (proof, io)
        };
        if args.memory {
            utils::track::flame_graph();
        }

        let proof_size = rmp_serde::to_vec(&proof)?.len();
        measure::set(HEADER_PROOF_SIZE, proof_size);

        verifier_ctx = verifier_ctx.with_max_context(max_ctx);
        info!("Verifying proof...");
        verifier_ctx
            .verify(proof, user_tokens, io)
            .expect("invalid proof");
        mem_snap("6.verify");
        mem_dump();
        if !args.distributed {
            measure::post_process(|metrics| {
                let Ok(proof_time) = metrics.get("prove_full").unwrap().parse::<usize>() else {
                    return;
                };
                let Ok(ctx_length) = metrics.get(HEADER_MAX_CONTEXT).unwrap().parse::<usize>()
                else {
                    return;
                };
                let token_per_second = ctx_length as f64 / (proof_time as f64 / 1000.0);
                metrics.insert("token/sec".to_string(), token_per_second.to_string());
            })?;
        }
        measure::to_csv(&bench_file)?;
    }

    Ok(())
}

/// 在当前进程整条 bench 流程的若干关键节点采一次内存快照, 汇总到一张表.
///
/// goal(任务一 "定位 256G 爆点"): 一次性把 trace 生成 / prove 各阶段的
/// 「当前 RSS」和「进程累计触碰峰值 RSS」都打出来, 不用逐个函数塞日志.
///
/// - 当前 RSS: 读 `/proc/self/statm`(Linux), 真实驻留内存, 可分段(只代表此刻).
/// - 进程峰值 RSS: `getrusage.ru_maxrss`, 整个进程生命周期只增不减, 只做兜底上限.
///   之所以两者都记: ru_maxrss 分不出"峰值出现在哪一段", 只有当前RSS能定位到节点.
#[derive(Default)]
struct MemProbe {
    /// snapshot(label) 每调用一次 push 一行; 结束由 dump() 打印成表
    samples: Mutex<Vec<(String, u64, u64)>>, // (label, rss_current, rss_peak_process)
}

/// /proc/self/statm 第二列 = 驻留页数. pagesize 需要 `getconf PAGESIZE`, 这里用 4KiB 假定;
/// 若不希望硬编码, 可用 libc::sysconf(_SC_PAGESIZE). 4KiB 在 x86_64/aarch64 Linux 上就是默认.
fn current_rss_bytes() -> u64 {
    static STATM: &str = "/proc/self/statm";
    let pages = std::fs::read_to_string(STATM)
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0);
    pages * 4096
}

impl MemProbe {
    fn snapshot(&self, label: &str) {
        let cur = current_rss_bytes();
        let peak = peak_rss_bytes();
        self.samples
            .lock()
            .unwrap()
            .push((label.to_string(), cur, peak));
    }

    /// 顺序打印每种内存的累计最大值及其出现节点, 一眼看清爆点在哪一段.
    fn dump(&self) {
        let samples = self.samples.lock().unwrap();
        if samples.is_empty() {
            return;
        }
        // 每个标签打印: 当前RSS(MB) 进程峰值RSS(MB) 相对上一段的当前RSS增量(MB)
        info!("===== 内存探测快照 (MemProbe) =====");
        info!(
            "  {:<46} {:>10} {:>12} {:>12}",
            "label", "cur_MB", "peak_MB", "cur_delta_MB"
        );
        let mut prev_cur = 0u64;
        let mut max_cur: (u64, &str) = (0, "");
        let mut max_peak: (u64, &str) = (0, "");
        for (label, cur, peak) in samples.iter() {
            let delta = cur.saturating_sub(prev_cur);
            if *cur > max_cur.0 {
                max_cur = (*cur, label);
            }
            if *peak > max_peak.0 {
                max_peak = (*peak, label);
            }
            info!(
                "  {:<46} {:>10.2} {:>12.2} {:>12.2}",
                label,
                *cur as f64 / 1e6,
                *peak as f64 / 1e6,
                delta as f64 / 1e6,
            );
            prev_cur = *cur;
        }
        info!(
            "  -> 当前RSS峰值节点: {} ({:.2} MB); 进程累计峰值: {} ({:.2} MB)",
            max_cur.1,
            max_cur.0 as f64 / 1e6,
            max_peak.1,
            max_peak.0 as f64 / 1e6,
        );
        info!("===== MemProbe end =====");
    }
}

thread_local! {
    /// 全局跟踪器, 供 bench 流程里各处 snapshot() 打点
    static MEM_PROBE: MemProbe = MemProbe::default();
}

fn mem_snap(label: &str) {
    MEM_PROBE.with(|p| p.snapshot(label));
}

fn mem_dump() {
    MEM_PROBE.with(|p| p.dump());
}

fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut r: rusage = std::mem::zeroed();
        getrusage(RUSAGE_SELF, &mut r);

        #[cfg(target_os = "linux")]
        {
            (r.ru_maxrss as u64) * 1024
        }

        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        {
            r.ru_maxrss as u64
        }
    }
}

fn parse_model(args: &LLMArgs, max_context: usize) -> anyhow::Result<(Driver<f32>, HFTokenizer)> {
    if let Some(ref gguf) = args.gguf {
        let model_path = file_cache::from_cache(gguf)?;
        match args.model {
            Model::GPT2 => {
                let model_type = GPT2::new();
                let raw_gguf = RawGGUF::new(model_path);
                Ok((
                    Driver::load_from_model(model_type, &raw_gguf, Some(max_context))?,
                    model_type.load_tokenizer(&raw_gguf)?,
                ))
            }
            _ => bail!("Model {:?} not supported for gguf", args.model),
        }
    } else if let Some(ref hf) = args.hf {
        let safe = RawSafeTensors::from_hugging_face_cached(hf)?;
        match args.model {
            Model::GPT2 => {
                let model_type = GPT2::new();
                Ok((
                    Driver::load_from_model(model_type, &safe, Some(max_context))?,
                    model_type.load_tokenizer(&safe)?,
                ))
            }
            Model::Gemma3 => {
                let model_type = Gemma3::new();
                Ok((
                    Driver::load_from_model(model_type, &safe, Some(max_context))?,
                    model_type.load_tokenizer(&safe)?,
                ))
            }
            Model::Llama2 => {
                let model_type = Llama2::new();
                Ok((
                    Driver::load_from_model(model_type, &safe, Some(max_context))?,
                    model_type.load_tokenizer(&safe)?,
                ))
            }
        }
    } else {
        bail!("Either gguf or hf must be provided");
    }
}

mod distributed {
    use std::collections::HashMap;

    use super::*;
    use anyhow::{anyhow, ensure};
    use dp_crypto::arkyper::transcript::blake3::Blake3Transcript;
    use tracing::debug;

    use zkml::{
        Element, IO, Proof, Prover,
        graph::{
            executor::{Executor, ThreadPoolExecutor},
            partition::PartitionScheduler,
            scheduler::ExecGraph,
        },
        iop::{
            chunking::ModelChunk,
            prover_graph::{LocalProverCtx, ProverGraphIO, ProverGraphNode},
        },
        model::Trace,
    };

    pub type T = Blake3Transcript;

    // Type of nodes of the graph to execute
    pub type Node<'a, 'b> = ProverGraphNode<'a, 'b, F, T, Pcs>;

    // Type of execution graph to be partitioned and executed in the workers
    pub type Graph<'a, 'b> = ExecGraph<Node<'a, 'b>, Color>;

    // Color is used to create the partitions, assign different nodes to different workers.
    // It can be usize or any other type such as IP address etc.
    pub type Color = usize;

    /// What a partition scheduler outputs
    pub type PartitionOutput<'a, 'b> = zkml::graph::partition::PartitionOutput<Node<'a, 'b>, Color>;

    const CHUNK_OUTPUT_SIZE: &str = "chunk_output_size";
    const CHUNK_INPUT_SIZE: &str = "chunk_input_size";

    fn run_next_partition<'a, 'b, E: Executor<Node<'a, 'b>, Color>>(
        schedulers: &mut HashMap<Color, PartitionScheduler<Node<'a, 'b>, Color, E>>,
    ) -> anyhow::Result<Option<PartitionOutput<'a, 'b>>> {
        let mut to_be_sent_outputs = Vec::new();
        let mut final_output = None;
        let mut done_schedulers = Vec::new();
        for (color, scheduler) in schedulers.iter_mut() {
            let outputs = scheduler.try_run_partition()?;
            for out in outputs {
                if let Some(to_node) = out.to {
                    let serialized_output = rmp_serde::to_vec(&out)?;
                    // ToDo: measure serialized_output
                    if to_node == 0 {
                        // this is data sent to the coordinator, so we add it to the set of data sent by workers
                        // to the coordinator
                        measure::accumulate_key(
                            CHUNK_OUTPUT_SIZE,
                            serialized_output.len(),
                            |a, b| a + b,
                        )?
                    } else if *color == 0 {
                        // this is data sent by the coordinator, so we add it to the set of data sent to the workers
                        measure::accumulate_key(
                            CHUNK_INPUT_SIZE,
                            serialized_output.len(),
                            |a, b| a + b,
                        )?
                    } else {
                        unreachable!(
                            "Data is either sent to coordinator or received by coordinator"
                        )
                    };
                    debug!("Node {} sending output to node {}", color, to_node);
                    to_be_sent_outputs.push((to_node, out));
                } else {
                    // we found the final output
                    final_output = Some(out);
                }
                if scheduler.is_done() {
                    done_schedulers.push(*color);
                }
            }
        }

        for (dest_color, out) in to_be_sent_outputs {
            schedulers
                .get_mut(&dest_color)
                .ok_or(anyhow!("Scheduler not found for color {dest_color}"))?
                .set_child_partition_output(out)?
        }

        for color in done_schedulers {
            schedulers.remove(&color);
        }

        Ok(final_output)
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn run_distributed(
        full_trace: Trace<Element>,
        driver: &Driver<Element>,
        prover_ctx: &ProverContext<F, Pcs>,
        chunks: Vec<ModelChunk>,
    ) -> anyhow::Result<(Proof<F, Pcs>, IO<F>)> {
        let io = full_trace.to_verifier_io()?;

        let graph: Graph = Prover::build_execution_graph(chunks)?;

        let inputs = Prover::graph_inputs(full_trace, &graph)?;

        ensure!(
            inputs.len() == 1,
            "Expected exactly one input node (coordinator split)"
        );

        let flat_inputs =
            inputs
                .into_iter()
                .fold(Vec::new(), |mut ios, (node_input, chunk_prover_io)| {
                    ios.push((node_input.node_id(), chunk_prover_io));
                    ios
                });
        let partitions = graph.partition_by_color(flat_inputs)?;

        let mut schedulers = partitions
            .into_iter()
            .map(|(color, partitions)| {
                let ctx = LocalProverCtx::new(prover_ctx, &driver.model);
                Ok((
                    color,
                    PartitionScheduler::<_, _, ThreadPoolExecutor>::new(partitions, ctx, ())?,
                ))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;

        let mut final_outputs = Vec::new();
        let peak_rss = peak_rss_bytes();
        while !schedulers.is_empty() {
            if let Some(final_output) = run_next_partition(&mut schedulers)? {
                final_outputs.push(final_output.output)
            };
        }
        let new_peak_rss = peak_rss_bytes();
        if new_peak_rss == peak_rss {
            warn!(
                "Cannot reliably measure peak memory consumption during proving, setting upper bound"
            );
        }
        measure::set(
            "prove_full_memory_peak",
            (new_peak_rss / 1024 / 1024).to_string(),
        );

        // Creates channels pairs to communicate with all other nodes
        ensure!(
            final_outputs.len() == 1,
            "Expected 1 outputs for the graph, {} outputs received",
            final_outputs.len()
        );
        let proof = match final_outputs.pop().unwrap() {
            ProverGraphIO::FinalProof(proof) => proof,
            _ => bail!("Invalid output type found after execution of ProverGraph"),
        };
        Ok((proof, io))
    }
}
