//! Priority queue abstractions for unified scheduling of bundles alongside normal transactions.
//!
//! This module introduces the (initial) skeleton for a `BundlePriorityQueue` which will
//! eventually be owned by BankingStage (or passed into its scheduler) so that bundles and
//! transactions can compete on a common reward-per-CU ordering.  For now this is a thin
//! in-memory heap with a trait object friendly interface.  The implementation purposefully
//! avoids any integration logic; wiring will happen in a later patch.

use {
    crate::immutable_deserialized_bundle::ImmutableDeserializedBundle,
    solana_bundle::SanitizedBundle,
    solana_pubkey::Pubkey,
    std::{
        cmp::Ordering,
        collections::BinaryHeap,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    },
};

/// Metadata describing a bundle for scheduling purposes.  All values are intended to be
/// pre-computed at ingestion time so the scheduler only performs cheap comparisons.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct BundleMeta {
    pub bundle_id: String,
    /// Sum of (transaction priority fees + extracted tips) in lamports.
    pub total_reward_lamports: u64,
    /// Estimated total compute units for all transactions (>=1 to avoid div-by-zero).
    pub total_cu_estimate: u64,
    /// Scaled reward-per-CU used for ordering (already multiplied by a constant factor).
    pub reward_per_cu_scaled: u64,
    /// Monotonic arrival timestamp (unix micros) – tie breaker for identical reward_per_cu.
    pub arrival_unix_us: u64,
    /// Canonical union of writable + readonly accounts touched by any tx in the bundle.
    /// (Will be populated in a later patch; kept empty for now.)
    pub union_accounts: Vec<Pubkey>,
    /// Number of contained transactions (helps with diagnostics / fairness heuristics).
    pub num_transactions: usize,
}

impl BundleMeta {
    #[allow(dead_code)]
    pub fn reward_per_cu_f64(&self) -> f64 {
        // Helper for debugging / metrics – NOT used in ordering.
        if self.total_cu_estimate == 0 {
            return 0.0;
        }
        (self.total_reward_lamports as f64) / (self.total_cu_estimate as f64)
    }
}

/// Opaque handle that owns the underlying bundle content.  The scheduler will move this handle
/// into execution after selecting the corresponding metadata.
#[derive(Debug)]
pub struct BundleHandle {
    inner: Arc<ImmutableDeserializedBundle>,
    // Optional pre-sanitized bundle (set when produced by BundleStage).
    sanitized: Option<Arc<SanitizedBundle>>,
}

impl Clone for BundleHandle {
    // Cheap clone (Arcs clone)
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            sanitized: self.sanitized.clone(),
        }
    }
}

impl BundleHandle {
    #[allow(dead_code)]
    pub fn new(inner: Arc<ImmutableDeserializedBundle>) -> Self {
        Self {
            inner,
            sanitized: None,
        }
    }
    #[allow(dead_code)]
    pub fn with_sanitized(
        inner: Arc<ImmutableDeserializedBundle>,
        sanitized: Arc<SanitizedBundle>,
    ) -> Self {
        Self {
            inner,
            sanitized: Some(sanitized),
        }
    }
    #[allow(dead_code)]
    pub fn bundle_id(&self) -> &str {
        self.inner.bundle_id()
    }
    #[allow(dead_code)]
    pub fn inner(&self) -> &Arc<ImmutableDeserializedBundle> {
        &self.inner
    }
    #[allow(dead_code)]
    pub fn sanitized(&self) -> Option<&Arc<SanitizedBundle>> {
        self.sanitized.as_ref()
    }
    #[allow(dead_code)]
    pub fn as_ref(&self) -> &ImmutableDeserializedBundle {
        &self.inner
    }
}

/// Trait exposed to the unified scheduler so it can *observe* and *conditionally* extract the
/// highest priority bundle without committing to removal until a predicate (typically identity
/// match) succeeds.  This enables a lock-attempt dance: peek meta -> attempt account union lock
/// -> remove only if still the best / predicate passes.
#[allow(dead_code)]
pub trait BundlePrioritySource: Send + Sync {
    fn peek_best(&self) -> Option<BundleMeta>;
    fn take_best_if<F>(&self, predicate: F) -> Option<BundleHandle>
    where
        F: FnOnce(&BundleMeta) -> bool;
    fn requeue(&self, handle: BundleHandle, meta: BundleMeta);
    fn len(&self) -> usize;
}

/// Internal pairing of meta + handle stored in the binary heap.
#[derive(Debug, Clone)]
struct StoredBundle {
    meta: BundleMeta,
    handle: BundleHandle,
}

impl StoredBundle {
    fn key_tuple(&self) -> (u64, u64, &str) {
        // Higher reward_per_cu_scaled first, then earlier arrival, then lexical id for stability.
        // BinaryHeap in std is a max-heap; we encode ordering accordingly.
        (
            self.meta.reward_per_cu_scaled,
            // invert arrival for max-heap (earlier arrival gets higher priority => larger second component)
            u64::MAX - self.meta.arrival_unix_us,
            self.meta.bundle_id.as_str(),
        )
    }
}

impl PartialEq for StoredBundle {
    fn eq(&self, other: &Self) -> bool {
        self.meta.bundle_id == other.meta.bundle_id
    }
}
impl Eq for StoredBundle {}
impl PartialOrd for StoredBundle {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for StoredBundle {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key_tuple().cmp(&other.key_tuple())
    }
}

/// Simple mutex-protected binary heap implementation.  All operations are O(log n) except `peek`.
#[derive(Default)]
pub struct BundlePriorityQueue {
    heap: Mutex<BinaryHeap<StoredBundle>>,
}

impl BundlePriorityQueue {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            heap: Mutex::new(BinaryHeap::new()),
        }
    }

    #[allow(dead_code)]
    pub fn insert(&self, meta: BundleMeta, handle: BundleHandle) {
        let mut g = self.heap.lock().unwrap();
        g.push(StoredBundle { meta, handle });
    }

    /// Helper for generating arrival timestamps (micros). Exposed so ingestion can stay consistent.
    #[allow(dead_code)]
    pub fn now_unix_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }
}

impl BundlePrioritySource for BundlePriorityQueue {
    fn peek_best(&self) -> Option<BundleMeta> {
        self.heap
            .lock()
            .ok()
            .and_then(|g| g.peek().map(|s| s.meta.clone()))
    }

    fn take_best_if<F>(&self, predicate: F) -> Option<BundleHandle>
    where
        F: FnOnce(&BundleMeta) -> bool,
    {
        let mut g = self.heap.lock().ok()?;
        if let Some(top) = g.peek() {
            if predicate(&top.meta) {
                let stored = g.pop().unwrap();
                return Some(stored.handle);
            }
        }
        None
    }

    fn requeue(&self, handle: BundleHandle, meta: BundleMeta) {
        let mut g = self.heap.lock().unwrap();
        g.push(StoredBundle { meta, handle });
    }

    fn len(&self) -> usize {
        self.heap.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::sync::Arc};
    use {
        crate::packet_bundle::PacketBundle, solana_hash::Hash, solana_keypair::Keypair,
        solana_perf::packet::BytesPacket, solana_signer::Signer,
        solana_system_transaction::transfer,
    };

    // Minimal fake bundle for constructing an ImmutableDeserializedBundle is non-trivial;
    // for now we only validate ordering mechanics by synthesizing metas over dummy Arc pointers.
    fn dummy_handle(_id: &str) -> BundleHandle {
        // Build a minimal immutable bundle using constructor with a single trivial transaction.

        let kp = Keypair::new();
        let tx = transfer(&kp, &kp.pubkey(), 1, Hash::default());
        let packet = BytesPacket::from_data(None, &tx).expect("packet");
        let mut pb = PacketBundle {
            batch: solana_perf::packet::PacketBatch::from(vec![packet]),
            bundle_id: String::new(),
        };
        let ib =
            crate::immutable_deserialized_bundle::ImmutableDeserializedBundle::new(&mut pb, None)
                .expect("immutable bundle");
        let handle = BundleHandle::new(Arc::new(ib));
        handle
    }

    #[test]
    fn ordering_prefers_higher_reward() {
        let q = BundlePriorityQueue::new();
        for (r, arrival) in [(10, 1), (50, 3), (30, 2)] {
            let meta = BundleMeta {
                bundle_id: format!("b{r}"),
                total_reward_lamports: r * 100,
                total_cu_estimate: 1,
                reward_per_cu_scaled: r as u64,
                arrival_unix_us: arrival,
                union_accounts: vec![],
                num_transactions: 1,
            };
            q.insert(meta, dummy_handle("dummy"));
        }
        let top = q.peek_best().unwrap();
        assert_eq!(top.reward_per_cu_scaled, 50);
    }
}
