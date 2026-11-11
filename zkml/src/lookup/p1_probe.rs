//! P1 猎捕探针（仅 `p1-debug` feature 生效，其余编译为空）。
//!
//! 背景：Chunk 内节点的 lookup witness 在 prove 线程统一生成（冻结 MLE），
//! 之后逐节点证明。若某节点的 step 张量在「forward → witness-gen」或
//! 「witness-gen → prove」窗口内被改写，冻结 witness 与 claim 就会不一致
//! （sumcheck 第 0 轮 claim 不符）。本模块用跨阶段 FNV 哈希对照抓出该窗口：
//!
//! - witness-gen 侧：`record_witness_input` 记录输入张量哈希；
//! - prove 侧：`check_prove_input` 重算哈希并对照，不一致即打印。
//!
//! 配合 linking sumcheck 的 `[p1-probe]`（witness vs claim）与 rmsnorm
//! 的 `[p1-rms]`（重算输出 vs 存储输出）探针，可将污染窗口三向定位。

#[cfg(feature = "p1-debug")]
mod imp {
    use crate::{Element, graph::NodeId};
    use std::cell::RefCell;
    use std::collections::HashMap;

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

    struct ProbeMap(HashMap<usize, (u64, Vec<usize>)>);

    thread_local! {
        static WITNESS_INPUT_HASHES: RefCell<ProbeMap> =
            RefCell::new(ProbeMap(HashMap::new()));
    }

    /// witness-gen 侧：记录节点输入张量的哈希与形状。
    pub fn record_witness_input(node_id: NodeId, data: &[Element], shape: &[usize]) {
        let hash = fnv_tensor(data, shape);
        WITNESS_INPUT_HASHES.with(|m| {
            m.borrow_mut()
                .0
                .insert(node_id.0, (hash, shape.to_vec()));
        });
    }

    /// prove 侧：对同一节点的输入张量重算哈希并与 witness-gen 记录对照。
    /// 无记录（非探针覆盖的节点类型）时静默跳过。
    pub fn check_prove_input(node_id: NodeId, data: &[Element], shape: &[usize]) {
        let recorded = WITNESS_INPUT_HASHES.with(|m| m.borrow_mut().0.remove(&node_id.0));
        if let Some((want_hash, want_shape)) = recorded {
            let now_hash = fnv_tensor(data, shape);
            if now_hash != want_hash {
                // 找出首个不一致元素位置
                eprintln!(
                    "[p1-hash] node N{}: 输入张量在 witness-gen 与 prove 之间被改写 \
                     (hash {want_hash:#x} -> {now_hash:#x}, shape {want_shape:?} -> {shape:?})",
                    node_id.0
                );
            }
        }
    }

    /// 清空本线程的探针表（每个 chunk prove 结束后调用，防跨 chunk 泄漏）。
    #[allow(dead_code)]
    pub fn clear_thread_probe() {
        WITNESS_INPUT_HASHES.with(|m| m.borrow_mut().0.clear());
    }
}

#[cfg(not(feature = "p1-debug"))]
mod imp {
    // 空实现：非 p1-debug 构建下零开销
    use crate::{Element, graph::NodeId};

    pub fn record_witness_input(_node_id: NodeId, _data: &[Element], _shape: &[usize]) {}
    pub fn check_prove_input(_node_id: NodeId, _data: &[Element], _shape: &[usize]) {}
    pub fn clear_thread_probe() {}
}

pub use imp::*;
