//! P1 猎捕探针（仅 `p1-debug` feature 生效，其余编译为空）。
//!
//! 背景：Chunk 内节点的 lookup witness 在 prove 线程统一生成（冻结 MLE），
//! 之后逐节点证明。若某节点的 step 张量在「forward → witness-gen」或
//! 「witness-gen → prove」窗口内被改写，冻结 witness 与 claim 就会不一致
//! （sumcheck 第 0 轮 claim 不符）。本模块用跨阶段 FNV 哈希对照抓出窗口：
//!
//! - forward（ingest）侧：`record_forward_input` 记录哈希（run 线程）；
//! - witness-gen 侧：`check_witness_input` 对照 forward（窗口 B：forward→
//!   witness-gen），并记录当前哈希；
//! - prove 侧：`check_prove_input` 对照 witness-gen（窗口 A：witness-gen→prove）。
//!
//! 三级哈希链布局无关（对比 witness 列布局的逐元素对照——witness 列是
//! 分块组织，与平铺张量不可直接比，实测全节点误报）。

#[cfg(feature = "p1-debug")]
mod imp {
    use crate::{Element, graph::NodeId};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    /// FNV-1a over the tensor's raw elements + shape (shape mixed in so a
    /// reshape alone is visible).
    pub(crate) fn fnv_tensor(data: &[Element], shape: &[usize]) -> u64 {
        let mut h = FNV_OFFSET;
        for &x in data {
            h ^= x as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        for &d in shape {
            h ^= d as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h
    }

    /// forward（ingest）时刻的输入哈希：run 线程写、prove 线程读 → Mutex。
    /// 键 = node id，值 = (hash, shape)。
    static FORWARD_INPUT_HASHES: Mutex<Option<HashMap<usize, (u64, Vec<usize>)>>> =
        Mutex::new(None);

    /// witness-gen 时刻的哈希：prove 线程内 witness-gen→prove 顺序使用。
    /// 键 = node id，值 = (hash, shape)。
    static WITNESS_INPUT_HASHES: Mutex<Option<HashMap<usize, (u64, Vec<usize>)>>> =
        Mutex::new(None);

    fn with_map<T>(
        map: &Mutex<Option<HashMap<usize, (u64, Vec<usize>)>>>,
        f: impl FnOnce(&mut HashMap<usize, (u64, Vec<usize>)>) -> T,
    ) -> Option<T> {
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        let m = guard.get_or_insert_with(HashMap::new);
        Some(f(m))
    }

    /// 探针映射键：(node, port) 组合，避免多输入节点端口互撞。
    fn key(node_id: NodeId, port: usize) -> usize {
        node_id.0 * 16 + port
    }

    /// forward（ingest）侧：记录节点输入张量的哈希与形状（run 线程调用）。
    pub fn record_forward_input(node_id: NodeId, port: usize, data: &[Element], shape: &[usize]) {
        let hash = fnv_tensor(data, shape);
        with_map(&FORWARD_INPUT_HASHES, |m| {
            m.insert(key(node_id, port), (hash, shape.to_vec()));
        });
    }

    /// witness-gen 侧：对照 forward 记录（窗口 B），并记录当前哈希（供
    /// prove 侧对照窗口 A）。
    pub fn check_witness_input(node_id: NodeId, port: usize, data: &[Element], shape: &[usize]) {
        let now_hash = fnv_tensor(data, shape);
        // 1) vs forward（窗口 B）
        let fwd = with_map(&FORWARD_INPUT_HASHES, |m| m.get(&key(node_id, port)).cloned());
        if let Some(Some((want_hash, want_shape))) = fwd {
            if now_hash != want_hash {
                eprintln!(
                    "[p1-fwd] node N{}: 输入张量在 forward 与 witness-gen 之间被改写 \
                     (hash {want_hash:#x} -> {now_hash:#x}, shape {want_shape:?} -> {shape:?})",
                    node_id.0
                );
            }
        }
        // 2) 记录供 prove 对照（窗口 A）
        with_map(&WITNESS_INPUT_HASHES, |m| {
            m.insert(key(node_id, port), (now_hash, shape.to_vec()));
        });
    }

    /// prove 侧：对同一节点的输入张量重算哈希并与 witness-gen 记录对照。
    /// 无记录（非探针覆盖的节点类型）时静默跳过。
    pub fn check_prove_input(node_id: NodeId, port: usize, data: &[Element], shape: &[usize]) {
        let recorded = with_map(&WITNESS_INPUT_HASHES, |m| m.remove(&key(node_id, port)));
        if let Some(Some((want_hash, want_shape))) = recorded {
            let now_hash = fnv_tensor(data, shape);
            if now_hash != want_hash {
                eprintln!(
                    "[p1-hash] node N{}: 输入张量在 witness-gen 与 prove 之间被改写 \
                     (hash {want_hash:#x} -> {now_hash:#x}, shape {want_shape:?} -> {shape:?})",
                    node_id.0
                );
            }
        }
    }

    /// 清空探针表（每个 chunk prove 结束后调用，防跨 chunk 泄漏）。
    #[allow(dead_code)]
    pub fn clear_thread_probe() {
        with_map(&WITNESS_INPUT_HASHES, |m| m.clear());
    }
}

#[cfg(not(feature = "p1-debug"))]
mod imp {
    // 空实现：非 p1-debug 构建下零开销
    use crate::{Element, graph::NodeId};

    pub fn record_forward_input(_n: NodeId, _p: usize, _d: &[Element], _s: &[usize]) {}
    pub fn check_witness_input(_n: NodeId, _p: usize, _d: &[Element], _s: &[usize]) {}
    pub fn check_prove_input(_n: NodeId, _p: usize, _d: &[Element], _s: &[usize]) {}
    #[allow(dead_code)]
    pub fn clear_thread_probe() {}
}

pub use imp::*;
