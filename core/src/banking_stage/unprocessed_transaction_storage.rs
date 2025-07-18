use {
    super::{
        consumer::Consumer,
        forward_packet_batches_by_accounts::ForwardPacketBatchesByAccounts,
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        latest_unprocessed_votes::{
            LatestUnprocessedVotes, LatestValidatorVotePacket, VoteBatchInsertionMetrics,
            VoteSource,
        },
        leader_slot_metrics::LeaderSlotMetricsTracker,
        multi_iterator_scanner::{MultiIteratorScanner, ProcessingDecision},
        read_write_account_set::ReadWriteAccountSet,
        unprocessed_packet_batches::{
            DeserializedPacket, PacketBatchInsertionMetrics, UnprocessedPacketBatches,
        },
        BankingStageStats, FilterForwardingResults, ForwardOption,
    },
    crate::{
        bundle_stage::bundle_stage_leader_metrics::BundleStageLeaderMetrics,
        immutable_deserialized_bundle::ImmutableDeserializedBundle,
    },
    agave_feature_set::{move_precompile_verification_to_svm, FeatureSet},
    arrayref::array_ref,
    itertools::Itertools,
    min_max_heap::MinMaxHeap,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_bundle::{
        bundle_execution::LoadAndExecuteBundleError, BundleExecutionError, SanitizedBundle,
    },
    solana_cost_model::cost_model::CostModel,
    solana_measure::measure_us,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::StaticMeta,
    },
    solana_sdk::{
        clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
        fee::FeeBudgetLimits,
        hash::Hash,
        instruction::CompiledInstruction,
        pubkey::Pubkey,
        saturating_add_assign, system_program,
        transaction::SanitizedTransaction,
    },
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{atomic::Ordering, Arc},
        time::Instant,
    },
};

// Step-size set to be 64, equal to the maximum batch/entry size. With the
// multi-iterator change, there's no point in getting larger batches of
// non-conflicting transactions.
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 64;
/// Maximum number of votes a single receive call will accept
const MAX_NUM_VOTES_RECEIVE: usize = 10_000;
const MAX_BUFFERED_BUNDLES: usize = 256;

#[derive(Debug)]
pub enum UnprocessedTransactionStorage {
    VoteStorage(VoteStorage),
    LocalTransactionStorage(ThreadLocalUnprocessedPackets),
    BundleStorage(BundleStorage),
}

#[derive(Debug)]
pub struct ThreadLocalUnprocessedPackets {
    unprocessed_packet_batches: UnprocessedPacketBatches,
    thread_type: ThreadType,
}

#[derive(Debug)]
pub struct VoteStorage {
    latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
    vote_source: VoteSource,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum ThreadType {
    Voting(VoteSource),
    Transactions,
    Bundles,
}

#[derive(Debug)]
pub enum InsertPacketBatchSummary {
    VoteBatchInsertionMetrics(VoteBatchInsertionMetrics),
    PacketBatchInsertionMetrics(PacketBatchInsertionMetrics),
}

impl InsertPacketBatchSummary {
    pub fn total_dropped_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => {
                metrics.num_dropped_gossip + metrics.num_dropped_tpu
            }
            Self::PacketBatchInsertionMetrics(metrics) => metrics.num_dropped_packets,
        }
    }

    pub fn dropped_gossip_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => metrics.num_dropped_gossip,
            _ => 0,
        }
    }

    pub fn dropped_tpu_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => metrics.num_dropped_tpu,
            _ => 0,
        }
    }
}

impl From<VoteBatchInsertionMetrics> for InsertPacketBatchSummary {
    fn from(metrics: VoteBatchInsertionMetrics) -> Self {
        Self::VoteBatchInsertionMetrics(metrics)
    }
}

impl From<PacketBatchInsertionMetrics> for InsertPacketBatchSummary {
    fn from(metrics: PacketBatchInsertionMetrics) -> Self {
        Self::PacketBatchInsertionMetrics(metrics)
    }
}

fn filter_processed_packets<'a, F>(
    retryable_transaction_indexes: impl Iterator<Item = &'a usize>,
    mut f: F,
) where
    F: FnMut(usize, usize),
{
    let mut prev_retryable_index = 0;
    for (i, retryable_index) in retryable_transaction_indexes.enumerate() {
        let start = if i == 0 { 0 } else { prev_retryable_index + 1 };

        let end = *retryable_index;
        prev_retryable_index = *retryable_index;

        if start < end {
            f(start, end)
        }
    }
}

/// Convenient wrapper for shared-state between banking stage processing and the
/// multi-iterator checking function.
pub struct ConsumeScannerPayload<'a> {
    pub reached_end_of_slot: bool,
    pub account_locks: ReadWriteAccountSet,
    pub sanitized_transactions: Vec<RuntimeTransaction<SanitizedTransaction>>,
    pub slot_metrics_tracker: &'a mut LeaderSlotMetricsTracker,
    pub message_hash_to_transaction: &'a mut HashMap<Hash, DeserializedPacket>,
    pub error_counters: TransactionErrorMetrics,
}

fn consume_scan_should_process_packet(
    bank: &Bank,
    banking_stage_stats: &BankingStageStats,
    packet: &ImmutableDeserializedPacket,
    payload: &mut ConsumeScannerPayload,
    blacklisted_accounts: &HashSet<Pubkey>,
) -> ProcessingDecision {
    // If end of the slot, return should process (quick loop after reached end of slot)
    if payload.reached_end_of_slot {
        return ProcessingDecision::Now;
    }

    // Try to sanitize the packet. Ignore deactivation slot since we are
    // immediately attempting to process the transaction.
    let (maybe_sanitized_transaction, sanitization_time_us) = measure_us!(packet
        .build_sanitized_transaction(
            bank.vote_only_bank(),
            bank,
            bank.get_reserved_account_keys(),
        )
        .map(|(tx, _deactivation_slot)| tx));

    payload
        .slot_metrics_tracker
        .increment_transactions_from_packets_us(sanitization_time_us);
    banking_stage_stats
        .packet_conversion_elapsed
        .fetch_add(sanitization_time_us, Ordering::Relaxed);

    if let Some(sanitized_transaction) = maybe_sanitized_transaction {
        let message = sanitized_transaction.message();

        // Check the number of locks and whether there are duplicates
        if validate_account_locks(
            message.account_keys(),
            bank.get_transaction_account_lock_limit(),
        )
        .is_err()
            || message
                .account_keys()
                .iter()
                .any(|key| blacklisted_accounts.contains(key))
        {
            payload
                .message_hash_to_transaction
                .remove(packet.message_hash());
            return ProcessingDecision::Never;
        }

        // Only check fee-payer if we can actually take locks
        // We do not immediately discard on check lock failures here,
        // because the priority guard requires that we always take locks
        // except in the cases of discarding transactions (i.e. `Never`).
        if payload.account_locks.check_locks(message)
            && Consumer::check_fee_payer_unlocked(
                bank,
                &sanitized_transaction,
                &mut payload.error_counters,
            )
            .is_err()
        {
            payload
                .message_hash_to_transaction
                .remove(packet.message_hash());
            return ProcessingDecision::Never;
        }

        // NOTE:
        //   This must be the last operation before adding the transaction to the
        //   sanitized_transactions vector. Otherwise, a transaction could
        //   be blocked by a transaction that did not take batch locks. This
        //   will lead to some transactions never being processed, and a
        //   mismatch in the priority-queue and hash map sizes.
        //
        // Always take locks during batch creation.
        // This prevents lower-priority transactions from taking locks
        // needed by higher-priority txs that were skipped by this check.
        if !payload.account_locks.take_locks(message) {
            return ProcessingDecision::Later;
        }

        payload.sanitized_transactions.push(sanitized_transaction);
        ProcessingDecision::Now
    } else {
        payload
            .message_hash_to_transaction
            .remove(packet.message_hash());
        ProcessingDecision::Never
    }
}

fn create_consume_multi_iterator<'a, 'b, F>(
    packets: &'a [Arc<ImmutableDeserializedPacket>],
    slot_metrics_tracker: &'b mut LeaderSlotMetricsTracker,
    message_hash_to_transaction: &'b mut HashMap<Hash, DeserializedPacket>,
    should_process_packet: F,
) -> MultiIteratorScanner<'a, Arc<ImmutableDeserializedPacket>, ConsumeScannerPayload<'b>, F>
where
    F: FnMut(
        &Arc<ImmutableDeserializedPacket>,
        &mut ConsumeScannerPayload<'b>,
    ) -> ProcessingDecision,
    'b: 'a,
{
    let payload = ConsumeScannerPayload {
        reached_end_of_slot: false,
        account_locks: ReadWriteAccountSet::default(),
        sanitized_transactions: Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE),
        slot_metrics_tracker,
        message_hash_to_transaction,
        error_counters: TransactionErrorMetrics::default(),
    };
    MultiIteratorScanner::new(
        packets,
        UNPROCESSED_BUFFER_STEP_SIZE,
        payload,
        should_process_packet,
    )
}

impl UnprocessedTransactionStorage {
    pub fn new_transaction_storage(
        unprocessed_packet_batches: UnprocessedPacketBatches,
        thread_type: ThreadType,
    ) -> Self {
        Self::LocalTransactionStorage(ThreadLocalUnprocessedPackets {
            unprocessed_packet_batches,
            thread_type,
        })
    }

    pub fn new_vote_storage(
        latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
        vote_source: VoteSource,
    ) -> Self {
        Self::VoteStorage(VoteStorage {
            latest_unprocessed_votes,
            vote_source,
        })
    }

    pub fn new_bundle_storage() -> Self {
        Self::BundleStorage(BundleStorage {
            last_update_slot: Slot::default(),
            unprocessed_bundle_storage: VecDeque::with_capacity(
                BundleStorage::BUNDLE_STORAGE_CAPACITY,
            ),
            cost_model_buffered_bundle_storage: VecDeque::with_capacity(
                BundleStorage::BUNDLE_STORAGE_CAPACITY,
            ),
        })
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.is_empty(),
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.is_empty(),
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                bundle_storage.is_empty()
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.len(),
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.len(),
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                bundle_storage.unprocessed_bundles_len()
                    + bundle_storage.cost_model_buffered_bundles_len()
            }
        }
    }

    pub fn get_min_priority(&self) -> Option<u64> {
        match self {
            Self::VoteStorage(_) => None,
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.get_min_compute_unit_price()
            }
            UnprocessedTransactionStorage::BundleStorage(_) => None,
        }
    }

    pub fn get_max_priority(&self) -> Option<u64> {
        match self {
            Self::VoteStorage(_) => None,
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.get_max_compute_unit_price()
            }
            UnprocessedTransactionStorage::BundleStorage(_) => None,
        }
    }

    /// Returns the maximum number of packets a receive should accept
    pub fn max_receive_size(&self) -> usize {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.max_receive_size(),
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.max_receive_size()
            }
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                bundle_storage.max_receive_size()
            }
        }
    }

    pub fn should_not_process(&self) -> bool {
        // The gossip vote thread does not need to process or forward any votes, that is
        // handled by the tpu vote thread
        if let Self::VoteStorage(vote_storage) = self {
            return matches!(vote_storage.vote_source, VoteSource::Gossip);
        }
        false
    }

    #[cfg(test)]
    pub fn iter(&mut self) -> impl Iterator<Item = &DeserializedPacket> {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.iter(),
            _ => panic!(),
        }
    }

    pub fn forward_option(&self) -> ForwardOption {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.forward_option(),
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.forward_option()
            }
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                bundle_storage.forward_option()
            }
        }
    }

    pub fn clear_forwarded_packets(&mut self) {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.clear(), // Since we set everything as forwarded this is the same
            Self::VoteStorage(vote_storage) => vote_storage.clear_forwarded_packets(),
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                let _ = bundle_storage.reset();
            }
        }
    }

    pub fn bundle_storage(&mut self) -> Option<&mut BundleStorage> {
        match self {
            UnprocessedTransactionStorage::BundleStorage(bundle_stoge) => Some(bundle_stoge),
            _ => None,
        }
    }

    pub(crate) fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> InsertPacketBatchSummary {
        match self {
            Self::VoteStorage(vote_storage) => {
                InsertPacketBatchSummary::from(vote_storage.insert_batch(deserialized_packets))
            }
            Self::LocalTransactionStorage(transaction_storage) => InsertPacketBatchSummary::from(
                transaction_storage.insert_batch(deserialized_packets),
            ),
            UnprocessedTransactionStorage::BundleStorage(_) => {
                panic!(
                    "bundles must be inserted using UnprocessedTransactionStorage::insert_bundle"
                )
            }
        }
    }

    pub fn filter_forwardable_packets_and_add_batches(
        &mut self,
        bank: Arc<Bank>,
        forward_packet_batches_by_accounts: &mut ForwardPacketBatchesByAccounts,
    ) -> FilterForwardingResults {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage
                .filter_forwardable_packets_and_add_batches(
                    bank,
                    forward_packet_batches_by_accounts,
                ),
            Self::VoteStorage(vote_storage) => vote_storage
                .filter_forwardable_packets_and_add_batches(
                    bank,
                    forward_packet_batches_by_accounts,
                ),
            UnprocessedTransactionStorage::BundleStorage(_) => {
                panic!("bundles are not forwarded between leaders")
            }
        }
    }

    /// The processing function takes a stream of packets ready to process, and returns the indices
    /// of the unprocessed packets that are eligible for retry. A return value of None means that
    /// all packets are unprocessed and eligible for retry.
    #[must_use]
    pub fn process_packets<F>(
        &mut self,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        processing_function: F,
        blacklisted_accounts: &HashSet<Pubkey>,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage
                .process_packets(
                    &bank,
                    banking_stage_stats,
                    slot_metrics_tracker,
                    processing_function,
                    blacklisted_accounts,
                ),
            Self::VoteStorage(vote_storage) => vote_storage.process_packets(
                bank,
                banking_stage_stats,
                slot_metrics_tracker,
                processing_function,
                blacklisted_accounts,
            ),
            UnprocessedTransactionStorage::BundleStorage(_) => panic!(
                "UnprocessedTransactionStorage::BundleStorage does not support processing packets"
            ),
        }
    }

    #[must_use]
    pub fn process_bundles<F>(
        &mut self,
        bank: Arc<Bank>,
        bundle_stage_leader_metrics: &mut BundleStageLeaderMetrics,
        blacklisted_accounts: &HashSet<Pubkey>,
        tip_accounts: &HashSet<Pubkey>,
        processing_function: F,
    ) -> bool
    where
        F: FnMut(
            &[(ImmutableDeserializedBundle, SanitizedBundle)],
            &mut BundleStageLeaderMetrics,
        ) -> Vec<Result<(), BundleExecutionError>>,
    {
        match self {
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => bundle_storage
                .process_bundles(
                    bank,
                    bundle_stage_leader_metrics,
                    blacklisted_accounts,
                    tip_accounts,
                    processing_function,
                ),
            _ => panic!("class does not support processing bundles"),
        }
    }

    /// Inserts bundles into storage. Only supported for UnprocessedTransactionStorage::BundleStorage
    pub(crate) fn insert_bundles(
        &mut self,
        deserialized_bundles: Vec<ImmutableDeserializedBundle>,
    ) -> InsertPacketBundlesSummary {
        match self {
            UnprocessedTransactionStorage::BundleStorage(bundle_storage) => {
                bundle_storage.insert_unprocessed_bundles(deserialized_bundles, true)
            }
            UnprocessedTransactionStorage::LocalTransactionStorage(_)
            | UnprocessedTransactionStorage::VoteStorage(_) => {
                panic!("UnprocessedTransactionStorage::insert_bundles only works for type UnprocessedTransactionStorage::BundleStorage");
            }
        }
    }

    pub(crate) fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        match self {
            Self::LocalTransactionStorage(_) => (),
            Self::VoteStorage(vote_storage) => vote_storage.cache_epoch_boundary_info(bank),
            UnprocessedTransactionStorage::BundleStorage(_) => (),
        }
    }
}

impl VoteStorage {
    fn is_empty(&self) -> bool {
        self.latest_unprocessed_votes.is_empty()
    }

    fn len(&self) -> usize {
        self.latest_unprocessed_votes.len()
    }

    fn max_receive_size(&self) -> usize {
        MAX_NUM_VOTES_RECEIVE
    }

    fn forward_option(&self) -> ForwardOption {
        match self.vote_source {
            VoteSource::Tpu => ForwardOption::ForwardTpuVote,
            VoteSource::Gossip => ForwardOption::NotForward,
        }
    }

    fn clear_forwarded_packets(&mut self) {
        self.latest_unprocessed_votes.clear_forwarded_packets();
    }

    fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> VoteBatchInsertionMetrics {
        self.latest_unprocessed_votes.insert_batch(
            deserialized_packets
                .into_iter()
                .filter_map(|deserialized_packet| {
                    LatestValidatorVotePacket::new_from_immutable(
                        Arc::new(deserialized_packet),
                        self.vote_source,
                        self.latest_unprocessed_votes
                            .should_deprecate_legacy_vote_ixs(),
                    )
                    .ok()
                }),
            false, // should_replenish_taken_votes
        )
    }

    fn filter_forwardable_packets_and_add_batches(
        &mut self,
        bank: Arc<Bank>,
        forward_packet_batches_by_accounts: &mut ForwardPacketBatchesByAccounts,
    ) -> FilterForwardingResults {
        if matches!(self.vote_source, VoteSource::Tpu) {
            let total_forwardable_packets = self
                .latest_unprocessed_votes
                .get_and_insert_forwardable_packets(bank, forward_packet_batches_by_accounts);
            return FilterForwardingResults {
                total_forwardable_packets,
                ..FilterForwardingResults::default()
            };
        }
        FilterForwardingResults::default()
    }

    // returns `true` if the end of slot is reached
    fn process_packets<F>(
        &mut self,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        mut processing_function: F,
        blacklisted_accounts: &HashSet<Pubkey>,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be processing transactions");
        }

        let should_process_packet =
            |packet: &Arc<ImmutableDeserializedPacket>, payload: &mut ConsumeScannerPayload| {
                consume_scan_should_process_packet(
                    &bank,
                    banking_stage_stats,
                    packet,
                    payload,
                    blacklisted_accounts,
                )
            };

        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self
            .latest_unprocessed_votes
            .drain_unprocessed(bank.clone());

        // vote storage does not have a message hash map, so pass in an empty one
        let mut dummy_message_hash_to_transaction = HashMap::new();
        let mut scanner = create_consume_multi_iterator(
            &all_vote_packets,
            slot_metrics_tracker,
            &mut dummy_message_hash_to_transaction,
            should_process_packet,
        );

        let deprecate_legacy_vote_ixs = self
            .latest_unprocessed_votes
            .should_deprecate_legacy_vote_ixs();

        while let Some((packets, payload)) = scanner.iterate() {
            let vote_packets = packets.iter().map(|p| (*p).clone()).collect_vec();

            if let Some(retryable_vote_indices) = processing_function(&vote_packets, payload) {
                self.latest_unprocessed_votes.insert_batch(
                    retryable_vote_indices.iter().filter_map(|i| {
                        LatestValidatorVotePacket::new_from_immutable(
                            vote_packets[*i].clone(),
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            } else {
                self.latest_unprocessed_votes.insert_batch(
                    vote_packets.into_iter().filter_map(|packet| {
                        LatestValidatorVotePacket::new_from_immutable(
                            packet,
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            }
        }

        scanner.finalize().payload.reached_end_of_slot
    }

    fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be checking epoch boundary");
        }
        self.latest_unprocessed_votes
            .cache_epoch_boundary_info(bank);
    }
}

impl ThreadLocalUnprocessedPackets {
    fn is_empty(&self) -> bool {
        self.unprocessed_packet_batches.is_empty()
    }

    pub fn thread_type(&self) -> ThreadType {
        self.thread_type
    }

    fn len(&self) -> usize {
        self.unprocessed_packet_batches.len()
    }

    pub fn get_min_compute_unit_price(&self) -> Option<u64> {
        self.unprocessed_packet_batches.get_min_compute_unit_price()
    }

    pub fn get_max_compute_unit_price(&self) -> Option<u64> {
        self.unprocessed_packet_batches.get_max_compute_unit_price()
    }

    fn max_receive_size(&self) -> usize {
        self.unprocessed_packet_batches.capacity() - self.unprocessed_packet_batches.len()
    }

    #[cfg(test)]
    fn iter(&mut self) -> impl Iterator<Item = &DeserializedPacket> {
        self.unprocessed_packet_batches.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut DeserializedPacket> {
        self.unprocessed_packet_batches.iter_mut()
    }

    fn forward_option(&self) -> ForwardOption {
        match self.thread_type {
            ThreadType::Transactions => ForwardOption::ForwardTransaction,
            ThreadType::Voting(VoteSource::Tpu) => ForwardOption::ForwardTpuVote,
            ThreadType::Voting(VoteSource::Gossip) => ForwardOption::NotForward,
            ThreadType::Bundles => ForwardOption::NotForward,
        }
    }

    fn clear(&mut self) {
        self.unprocessed_packet_batches.clear();
    }

    fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> PacketBatchInsertionMetrics {
        self.unprocessed_packet_batches.insert_batch(
            deserialized_packets
                .into_iter()
                .map(DeserializedPacket::from_immutable_section),
        )
    }

    /// Filter out packets that fail to sanitize, or are no longer valid (could be
    /// too old, a duplicate of something already processed). Doing this in batches to avoid
    /// checking bank's blockhash and status cache per transaction which could be bad for performance.
    /// Added valid and sanitized packets to forwarding queue.
    fn filter_forwardable_packets_and_add_batches(
        &mut self,
        bank: Arc<Bank>,
        forward_buffer: &mut ForwardPacketBatchesByAccounts,
    ) -> FilterForwardingResults {
        let mut total_forwardable_packets: usize = 0;
        let mut total_packet_conversion_us: u64 = 0;
        let mut total_filter_packets_us: u64 = 0;
        let mut total_dropped_packets: usize = 0;

        let mut original_priority_queue = self.take_priority_queue();
        let original_capacity = original_priority_queue.capacity();
        let mut new_priority_queue = MinMaxHeap::with_capacity(original_capacity);

        // indicates if `forward_buffer` still accept more packets, see details at
        // `ForwardPacketBatchesByAccounts.rs`.
        let mut accepting_packets = true;
        // batch iterate through self.unprocessed_packet_batches in desc priority order
        new_priority_queue.extend(
            original_priority_queue
                .drain_desc()
                .chunks(UNPROCESSED_BUFFER_STEP_SIZE)
                .into_iter()
                .flat_map(|packets_to_process| {
                    // Only process packets not yet forwarded
                    let (forwarded_packets, packets_to_forward) =
                        self.prepare_packets_to_forward(packets_to_process);

                    [
                        forwarded_packets,
                        if accepting_packets {
                            let (
                                (sanitized_transactions, transaction_to_packet_indexes),
                                packet_conversion_us,
                            ) = measure_us!(self.sanitize_unforwarded_packets(
                                &packets_to_forward,
                                &bank,
                                &mut total_dropped_packets
                            ));
                            saturating_add_assign!(
                                total_packet_conversion_us,
                                packet_conversion_us
                            );

                            let (forwardable_transaction_indexes, filter_packets_us) =
                                measure_us!(Self::filter_invalid_transactions(
                                    &sanitized_transactions,
                                    &bank,
                                    &mut total_dropped_packets
                                ));
                            saturating_add_assign!(total_filter_packets_us, filter_packets_us);
                            saturating_add_assign!(
                                total_forwardable_packets,
                                forwardable_transaction_indexes.len()
                            );

                            let accepted_packet_indexes =
                                Self::add_filtered_packets_to_forward_buffer(
                                    forward_buffer,
                                    &packets_to_forward,
                                    &sanitized_transactions,
                                    &transaction_to_packet_indexes,
                                    &forwardable_transaction_indexes,
                                    &mut total_dropped_packets,
                                    &bank.feature_set,
                                );
                            accepting_packets = accepted_packet_indexes.len()
                                == forwardable_transaction_indexes.len();

                            self.unprocessed_packet_batches
                                .mark_accepted_packets_as_forwarded(
                                    &packets_to_forward,
                                    &accepted_packet_indexes,
                                );

                            Self::collect_retained_packets(
                                &mut self.unprocessed_packet_batches.message_hash_to_transaction,
                                &packets_to_forward,
                                &Self::prepare_filtered_packet_indexes(
                                    &transaction_to_packet_indexes,
                                    &forwardable_transaction_indexes,
                                ),
                            )
                        } else {
                            // skip sanitizing and filtering if not longer able to add more packets for forwarding
                            saturating_add_assign!(total_dropped_packets, packets_to_forward.len());
                            packets_to_forward
                        },
                    ]
                    .concat()
                }),
        );

        // replace packet priority queue
        self.unprocessed_packet_batches.packet_priority_queue = new_priority_queue;
        self.verify_priority_queue(original_capacity);

        // Assert unprocessed queue is still consistent
        assert_eq!(
            self.unprocessed_packet_batches.packet_priority_queue.len(),
            self.unprocessed_packet_batches
                .message_hash_to_transaction
                .len()
        );

        FilterForwardingResults {
            total_forwardable_packets,
            total_dropped_packets,
            total_packet_conversion_us,
            total_filter_packets_us,
        }
    }

    /// Take self.unprocessed_packet_batches's priority_queue out, leave empty MinMaxHeap in its place.
    fn take_priority_queue(&mut self) -> MinMaxHeap<Arc<ImmutableDeserializedPacket>> {
        std::mem::replace(
            &mut self.unprocessed_packet_batches.packet_priority_queue,
            MinMaxHeap::new(), // <-- no need to reserve capacity as we will replace this
        )
    }

    /// Verify that the priority queue and map are consistent and that original capacity is maintained.
    fn verify_priority_queue(&self, original_capacity: usize) {
        // Assert unprocessed queue is still consistent and maintains original capacity
        assert_eq!(
            self.unprocessed_packet_batches
                .packet_priority_queue
                .capacity(),
            original_capacity
        );
        assert_eq!(
            self.unprocessed_packet_batches.packet_priority_queue.len(),
            self.unprocessed_packet_batches
                .message_hash_to_transaction
                .len()
        );
    }

    /// sanitize un-forwarded packet into SanitizedTransaction for validation and forwarding.
    fn sanitize_unforwarded_packets(
        &mut self,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        bank: &Bank,
        total_dropped_packets: &mut usize,
    ) -> (Vec<RuntimeTransaction<SanitizedTransaction>>, Vec<usize>) {
        // Get ref of ImmutableDeserializedPacket
        let deserialized_packets = packets_to_process.iter().map(|p| &**p);
        let (transactions, transaction_to_packet_indexes): (Vec<_>, Vec<_>) = deserialized_packets
            .enumerate()
            .filter_map(|(packet_index, deserialized_packet)| {
                deserialized_packet
                    .build_sanitized_transaction(
                        bank.vote_only_bank(),
                        bank,
                        bank.get_reserved_account_keys(),
                    )
                    .map(|(transaction, _deactivation_slot)| (transaction, packet_index))
            })
            .unzip();

        let filtered_count = packets_to_process.len().saturating_sub(transactions.len());
        saturating_add_assign!(*total_dropped_packets, filtered_count);

        (transactions, transaction_to_packet_indexes)
    }

    /// Checks sanitized transactions against bank, returns valid transaction indexes
    fn filter_invalid_transactions(
        transactions: &[RuntimeTransaction<SanitizedTransaction>],
        bank: &Bank,
        total_dropped_packets: &mut usize,
    ) -> Vec<usize> {
        let filter = vec![Ok(()); transactions.len()];
        let results = bank.check_transactions_with_forwarding_delay(
            transactions,
            &filter,
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
        );

        let filtered_count = transactions.len().saturating_sub(results.len());
        saturating_add_assign!(*total_dropped_packets, filtered_count);

        results
            .iter()
            .enumerate()
            .filter_map(|(tx_index, result)| result.as_ref().ok().map(|_| tx_index))
            .collect_vec()
    }

    fn prepare_filtered_packet_indexes(
        transaction_to_packet_indexes: &[usize],
        retained_transaction_indexes: &[usize],
    ) -> Vec<usize> {
        retained_transaction_indexes
            .iter()
            .map(|tx_index| transaction_to_packet_indexes[*tx_index])
            .collect_vec()
    }

    /// try to add filtered forwardable and valid packets to forward buffer;
    /// returns vector of packet indexes that were accepted for forwarding.
    fn add_filtered_packets_to_forward_buffer(
        forward_buffer: &mut ForwardPacketBatchesByAccounts,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        transactions: &[RuntimeTransaction<SanitizedTransaction>],
        transaction_to_packet_indexes: &[usize],
        forwardable_transaction_indexes: &[usize],
        total_dropped_packets: &mut usize,
        feature_set: &FeatureSet,
    ) -> Vec<usize> {
        let mut added_packets_count: usize = 0;
        let mut accepted_packet_indexes = Vec::with_capacity(transaction_to_packet_indexes.len());
        for forwardable_transaction_index in forwardable_transaction_indexes {
            let sanitized_transaction = &transactions[*forwardable_transaction_index];
            let forwardable_packet_index =
                transaction_to_packet_indexes[*forwardable_transaction_index];
            let immutable_deserialized_packet =
                packets_to_process[forwardable_packet_index].clone();
            if !forward_buffer.try_add_packet(
                sanitized_transaction,
                immutable_deserialized_packet,
                feature_set,
            ) {
                break;
            }
            accepted_packet_indexes.push(forwardable_packet_index);
            saturating_add_assign!(added_packets_count, 1);
        }

        let filtered_count = forwardable_transaction_indexes
            .len()
            .saturating_sub(added_packets_count);
        saturating_add_assign!(*total_dropped_packets, filtered_count);

        accepted_packet_indexes
    }

    fn collect_retained_packets(
        message_hash_to_transaction: &mut HashMap<Hash, DeserializedPacket>,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        retained_packet_indexes: &[usize],
    ) -> Vec<Arc<ImmutableDeserializedPacket>> {
        Self::remove_non_retained_packets(
            message_hash_to_transaction,
            packets_to_process,
            retained_packet_indexes,
        );
        retained_packet_indexes
            .iter()
            .map(|i| packets_to_process[*i].clone())
            .collect_vec()
    }

    /// remove packets from UnprocessedPacketBatches.message_hash_to_transaction after they have
    /// been removed from UnprocessedPacketBatches.packet_priority_queue
    fn remove_non_retained_packets(
        message_hash_to_transaction: &mut HashMap<Hash, DeserializedPacket>,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        retained_packet_indexes: &[usize],
    ) {
        filter_processed_packets(
            retained_packet_indexes
                .iter()
                .chain(std::iter::once(&packets_to_process.len())),
            |start, end| {
                for processed_packet in &packets_to_process[start..end] {
                    message_hash_to_transaction.remove(processed_packet.message_hash());
                }
            },
        )
    }

    // returns `true` if reached end of slot
    fn process_packets<F>(
        &mut self,
        bank: &Bank,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        mut processing_function: F,
        blacklisted_accounts: &HashSet<Pubkey>,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        let mut retryable_packets = self.take_priority_queue();
        let original_capacity = retryable_packets.capacity();
        let mut new_retryable_packets = MinMaxHeap::with_capacity(original_capacity);
        let all_packets_to_process = retryable_packets.drain_desc().collect_vec();

        let should_process_packet =
            |packet: &Arc<ImmutableDeserializedPacket>, payload: &mut ConsumeScannerPayload| {
                consume_scan_should_process_packet(
                    bank,
                    banking_stage_stats,
                    packet,
                    payload,
                    blacklisted_accounts,
                )
            };
        let mut scanner = create_consume_multi_iterator(
            &all_packets_to_process,
            slot_metrics_tracker,
            &mut self.unprocessed_packet_batches.message_hash_to_transaction,
            should_process_packet,
        );

        while let Some((packets_to_process, payload)) = scanner.iterate() {
            let packets_to_process = packets_to_process
                .iter()
                .map(|p| (*p).clone())
                .collect_vec();
            let retryable_packets = if let Some(retryable_transaction_indexes) =
                processing_function(&packets_to_process, payload)
            {
                Self::collect_retained_packets(
                    payload.message_hash_to_transaction,
                    &packets_to_process,
                    &retryable_transaction_indexes,
                )
            } else {
                packets_to_process
            };

            new_retryable_packets.extend(retryable_packets);
        }

        let reached_end_of_slot = scanner.finalize().payload.reached_end_of_slot;

        self.unprocessed_packet_batches.packet_priority_queue = new_retryable_packets;
        self.verify_priority_queue(original_capacity);

        reached_end_of_slot
    }

    /// Prepare a chunk of packets for forwarding, filter out already forwarded packets while
    /// counting tracers.
    /// Returns Vec of unforwarded packets, and Vec<bool> of same size each indicates corresponding
    /// packet is tracer packet.
    fn prepare_packets_to_forward(
        &self,
        packets_to_forward: impl Iterator<Item = Arc<ImmutableDeserializedPacket>>,
    ) -> (
        Vec<Arc<ImmutableDeserializedPacket>>,
        Vec<Arc<ImmutableDeserializedPacket>>,
    ) {
        let mut forwarded_packets: Vec<Arc<ImmutableDeserializedPacket>> = vec![];
        let forwardable_packets = packets_to_forward
            .into_iter()
            .filter_map(|immutable_deserialized_packet| {
                if !self
                    .unprocessed_packet_batches
                    .is_forwarded(&immutable_deserialized_packet)
                {
                    Some(immutable_deserialized_packet)
                } else {
                    forwarded_packets.push(immutable_deserialized_packet);
                    None
                }
            })
            .collect();

        (forwarded_packets, forwardable_packets)
    }
}

pub struct InsertPacketBundlesSummary {
    pub insert_packets_summary: InsertPacketBatchSummary,
    pub num_bundles_inserted: usize,
    pub num_packets_inserted: usize,
    pub num_bundles_dropped: usize,
}

/// Bundle storage has two deques: one for unprocessed bundles and another for ones that exceeded
/// the cost model and need to get retried next slot.
#[derive(Debug)]
pub struct BundleStorage {
    last_update_slot: Slot,
    pub unprocessed_bundle_storage: VecDeque<ImmutableDeserializedBundle>,
    // Storage for bundles that exceeded the cost model for the slot they were last attempted
    // execution on
    pub cost_model_buffered_bundle_storage: VecDeque<ImmutableDeserializedBundle>,
}

impl BundleStorage {
    pub const BUNDLE_STORAGE_CAPACITY: usize = 1000;
    fn is_empty(&self) -> bool {
        self.unprocessed_bundle_storage.is_empty()
    }

    pub fn unprocessed_bundles_len(&self) -> usize {
        self.unprocessed_bundle_storage.len()
    }

    pub fn unprocessed_packets_len(&self) -> usize {
        self.unprocessed_bundle_storage
            .iter()
            .map(|b| b.len())
            .sum::<usize>()
    }

    pub(crate) fn cost_model_buffered_bundles_len(&self) -> usize {
        self.cost_model_buffered_bundle_storage.len()
    }

    pub(crate) fn cost_model_buffered_packets_len(&self) -> usize {
        self.cost_model_buffered_bundle_storage
            .iter()
            .map(|b| b.len())
            .sum()
    }

    pub(crate) fn max_receive_size(&self) -> usize {
        MAX_BUFFERED_BUNDLES - self.unprocessed_bundle_storage.len()
    }

    fn forward_option(&self) -> ForwardOption {
        ForwardOption::NotForward
    }

    /// Returns the number of unprocessed bundles + cost model buffered cleared
    pub fn reset(&mut self) -> (usize, usize) {
        let num_unprocessed_bundles = self.unprocessed_bundle_storage.len();
        let num_cost_model_buffered_bundles = self.cost_model_buffered_bundle_storage.len();
        self.unprocessed_bundle_storage.clear();
        self.cost_model_buffered_bundle_storage.clear();
        (num_unprocessed_bundles, num_cost_model_buffered_bundles)
    }

    fn insert_bundles(
        deque: &mut VecDeque<ImmutableDeserializedBundle>,
        deserialized_bundles: Vec<ImmutableDeserializedBundle>,
        push_back: bool,
    ) -> InsertPacketBundlesSummary {
        // deque should be initialized with size [Self::BUNDLE_STORAGE_CAPACITY]
        let deque_free_space = Self::BUNDLE_STORAGE_CAPACITY
            .checked_sub(deque.len())
            .unwrap();
        let bundles_to_insert_count = std::cmp::min(deque_free_space, deserialized_bundles.len());
        let num_bundles_dropped = deserialized_bundles
            .len()
            .checked_sub(bundles_to_insert_count)
            .unwrap();
        let num_packets_inserted = deserialized_bundles
            .iter()
            .take(bundles_to_insert_count)
            .map(|b| b.len())
            .sum::<usize>();
        let num_packets_dropped = deserialized_bundles
            .iter()
            .skip(bundles_to_insert_count)
            .map(|b| b.len())
            .sum::<usize>();

        let to_insert = deserialized_bundles
            .into_iter()
            .take(bundles_to_insert_count);
        if push_back {
            deque.extend(to_insert)
        } else {
            to_insert.for_each(|b| deque.push_front(b));
        }

        InsertPacketBundlesSummary {
            insert_packets_summary: PacketBatchInsertionMetrics {
                num_dropped_packets: num_packets_dropped,
            }
            .into(),
            num_bundles_inserted: bundles_to_insert_count,
            num_packets_inserted,
            num_bundles_dropped,
        }
    }

    fn push_front_unprocessed_bundles(
        &mut self,
        deserialized_bundles: Vec<ImmutableDeserializedBundle>,
    ) -> InsertPacketBundlesSummary {
        Self::insert_bundles(
            &mut self.unprocessed_bundle_storage,
            deserialized_bundles,
            false,
        )
    }

    fn push_back_cost_model_buffered_bundles(
        &mut self,
        deserialized_bundles: Vec<ImmutableDeserializedBundle>,
    ) -> InsertPacketBundlesSummary {
        Self::insert_bundles(
            &mut self.cost_model_buffered_bundle_storage,
            deserialized_bundles,
            true,
        )
    }

    fn insert_unprocessed_bundles(
        &mut self,
        deserialized_bundles: Vec<ImmutableDeserializedBundle>,
        push_back: bool,
    ) -> InsertPacketBundlesSummary {
        Self::insert_bundles(
            &mut self.unprocessed_bundle_storage,
            deserialized_bundles,
            push_back,
        )
    }

    /// Drains bundles from the queue, sanitizes them to prepare for execution, executes them by
    /// calling `processing_function`, then potentially rebuffer them.
    pub fn process_bundles<F>(
        &mut self,
        bank: Arc<Bank>,
        bundle_stage_leader_metrics: &mut BundleStageLeaderMetrics,
        blacklisted_accounts: &HashSet<Pubkey>,
        tip_accounts: &HashSet<Pubkey>,
        mut processing_function: F,
    ) -> bool
    where
        F: FnMut(
            &[(ImmutableDeserializedBundle, SanitizedBundle)],
            &mut BundleStageLeaderMetrics,
        ) -> Vec<Result<(), BundleExecutionError>>,
    {
        let sanitized_bundles = self.drain_and_sanitize_bundles(
            bank,
            bundle_stage_leader_metrics,
            blacklisted_accounts,
            tip_accounts,
        );

        debug!("processing {} bundles", sanitized_bundles.len());
        let bundle_execution_results =
            processing_function(&sanitized_bundles, bundle_stage_leader_metrics);

        let mut is_slot_over = false;

        let mut rebuffered_bundles = Vec::new();

        sanitized_bundles
            .into_iter()
            .zip(bundle_execution_results)
            .for_each(
                |((deserialized_bundle, sanitized_bundle), result)| match result {
                    Ok(_) => {
                        debug!("bundle={} executed ok", sanitized_bundle.bundle_id);
                        // yippee
                    }
                    Err(BundleExecutionError::PohRecordError(e)) => {
                        // buffer the bundle to the front of the queue to be attempted next slot
                        debug!(
                            "bundle={} poh record error: {e:?}",
                            sanitized_bundle.bundle_id
                        );
                        rebuffered_bundles.push(deserialized_bundle);
                        is_slot_over = true;
                    }
                    Err(BundleExecutionError::BankProcessingTimeLimitReached) => {
                        // buffer the bundle to the front of the queue to be attempted next slot
                        debug!("bundle={} bank processing done", sanitized_bundle.bundle_id);
                        rebuffered_bundles.push(deserialized_bundle);
                        is_slot_over = true;
                    }
                    Err(BundleExecutionError::ExceedsCostModel) => {
                        // cost model buffered bundles contain most recent bundles at the front of the queue
                        debug!(
                            "bundle={} exceeds cost model, rebuffering",
                            sanitized_bundle.bundle_id
                        );
                        self.push_back_cost_model_buffered_bundles(vec![deserialized_bundle]);
                    }
                    Err(BundleExecutionError::TransactionFailure(
                        LoadAndExecuteBundleError::ProcessingTimeExceeded(_),
                    )) => {
                        // these are treated the same as exceeds cost model and are rebuferred to be completed
                        // at the beginning of the next slot
                        debug!(
                            "bundle={} processing time exceeded, rebuffering",
                            sanitized_bundle.bundle_id
                        );
                        self.push_back_cost_model_buffered_bundles(vec![deserialized_bundle]);
                    }
                    Err(BundleExecutionError::TransactionFailure(e)) => {
                        debug!(
                            "bundle={} execution error: {:?}",
                            sanitized_bundle.bundle_id, e
                        );
                        // do nothing
                    }
                    Err(BundleExecutionError::TipError(e)) => {
                        debug!("bundle={} tip error: {}", sanitized_bundle.bundle_id, e);
                        // Tip errors are _typically_ due to misconfiguration (except for poh record error, bank processing error, exceeds cost model)
                        // in order to prevent buffering too many bundles, we'll just drop the bundle
                    }
                    Err(BundleExecutionError::LockError) => {
                        // lock errors are irrecoverable due to malformed transactions
                        debug!("bundle={} lock error", sanitized_bundle.bundle_id);
                    }
                    // NB: Tip cutoff is static & front-runs will never succeed.
                    Err(BundleExecutionError::FrontRun) => {}
                },
            );

        // rebuffered bundles are pushed onto deque in reverse order so the first bundle is at the front
        for bundle in rebuffered_bundles.into_iter().rev() {
            self.push_front_unprocessed_bundles(vec![bundle]);
        }

        is_slot_over
    }

    /// Drains the unprocessed_bundle_storage, converting bundle packets into SanitizedBundles
    fn drain_and_sanitize_bundles(
        &mut self,
        bank: Arc<Bank>,
        bundle_stage_leader_metrics: &mut BundleStageLeaderMetrics,
        blacklisted_accounts: &HashSet<Pubkey>,
        tip_accounts: &HashSet<Pubkey>,
    ) -> Vec<(ImmutableDeserializedBundle, SanitizedBundle)> {
        let mut error_metrics = TransactionErrorMetrics::default();

        let start = Instant::now();

        let mut sanitized_bundles = Vec::new();

        let move_precompile_verification_to_svm = bank
            .feature_set
            .is_active(&move_precompile_verification_to_svm::id());

        // on new slot, drain anything that was buffered from last slot
        if bank.slot() != self.last_update_slot {
            sanitized_bundles.extend(
                self.cost_model_buffered_bundle_storage
                    .drain(..)
                    .filter_map(|packet_bundle| {
                        let r = packet_bundle.build_sanitized_bundle(
                            &bank,
                            blacklisted_accounts,
                            &mut error_metrics,
                            move_precompile_verification_to_svm,
                        );
                        bundle_stage_leader_metrics
                            .bundle_stage_metrics_tracker()
                            .increment_sanitize_transaction_result(&r);

                        match r {
                            Ok(sanitized_bundle) => Some((packet_bundle, sanitized_bundle)),
                            Err(e) => {
                                debug!(
                                    "bundle id: {} error sanitizing: {}",
                                    packet_bundle.bundle_id(),
                                    e
                                );
                                None
                            }
                        }
                    }),
            );

            self.last_update_slot = bank.slot();
        }

        sanitized_bundles.extend(self.unprocessed_bundle_storage.drain(..).filter_map(
            |packet_bundle| {
                let r = packet_bundle.build_sanitized_bundle(
                    &bank,
                    blacklisted_accounts,
                    &mut error_metrics,
                    move_precompile_verification_to_svm,
                );
                bundle_stage_leader_metrics
                    .bundle_stage_metrics_tracker()
                    .increment_sanitize_transaction_result(&r);
                match r {
                    Ok(sanitized_bundle) => Some((packet_bundle, sanitized_bundle)),
                    Err(e) => {
                        debug!(
                            "bundle id: {} error sanitizing: {}",
                            packet_bundle.bundle_id(),
                            e
                        );
                        None
                    }
                }
            },
        ));
        let mut priority_counter = 0;
        sanitized_bundles.sort_by_cached_key(|(immutable_bundle, sanitized_bundle)| {
            Self::calculate_bundle_priority(
                immutable_bundle,
                sanitized_bundle,
                &bank,
                &mut priority_counter,
                tip_accounts,
            )
        });

        let elapsed = start.elapsed().as_micros() as u64;
        bundle_stage_leader_metrics
            .bundle_stage_metrics_tracker()
            .increment_sanitize_bundle_elapsed_us(elapsed);
        bundle_stage_leader_metrics
            .leader_slot_metrics_tracker()
            .increment_transactions_from_packets_us(elapsed);
        bundle_stage_leader_metrics
            .leader_slot_metrics_tracker()
            .accumulate_transaction_errors(&error_metrics);

        sanitized_bundles
    }

    fn calculate_bundle_priority(
        immutable_bundle: &ImmutableDeserializedBundle,
        sanitized_bundle: &SanitizedBundle,
        bank: &Bank,
        priority_counter: &mut u64,
        tip_accounts: &HashSet<Pubkey>,
    ) -> (std::cmp::Reverse<u64>, u64) {
        let total_cu_cost: u64 = sanitized_bundle
            .transactions
            .iter()
            .map(|tx| CostModel::calculate_cost(tx, &bank.feature_set).sum())
            .sum();

        let reward_from_tx: u64 = sanitized_bundle
            .transactions
            .iter()
            .map(|tx| {
                let limits = tx
                    .compute_budget_instruction_details()
                    .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)
                    .unwrap_or_default();
                bank.calculate_reward_for_transaction(tx, &FeeBudgetLimits::from(limits))
            })
            .sum();

        let reward_from_tips: u64 = immutable_bundle
            .packets()
            .iter()
            .map(|packets| Self::extract_tips_from_packet(packets, tip_accounts))
            .sum();

        let total_reward = reward_from_tx.saturating_add(reward_from_tips);
        const MULTIPLIER: u64 = 1_000_000;
        let priority = total_reward.saturating_mul(MULTIPLIER) / total_cu_cost.max(1);
        *priority_counter = priority_counter.wrapping_add(1);

        (std::cmp::Reverse(priority), *priority_counter)
    }

    pub fn extract_tips_from_packet(
        packet: &ImmutableDeserializedPacket,
        tip_accounts: &HashSet<Pubkey>,
    ) -> u64 {
        let message = packet.transaction().get_message();
        let account_keys = message.message.static_account_keys();
        message
            .program_instructions_iter()
            .filter_map(|(pid, ix)| Self::extract_transfer(account_keys, pid, ix))
            .filter(|(dest, _)| tip_accounts.contains(dest))
            .map(|(_, amount)| amount)
            .sum()
    }

    fn extract_transfer<'a>(
        account_keys: &'a [Pubkey],
        program_id: &Pubkey,
        ix: &CompiledInstruction,
    ) -> Option<(&'a Pubkey, u64)> {
        if program_id == &system_program::ID
            && ix.data.len() >= 12
            && u32::from_le_bytes(*array_ref![&ix.data, 0, 4]) == 2
        {
            let destination = account_keys.get(*ix.accounts.get(1)? as usize)?;
            let amount = u64::from_le_bytes(*array_ref![ix.data, 4, 8]);

            Some((destination, amount))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::packet_bundle::PacketBundle,
        itertools::iproduct,
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_perf::packet::{Packet, PacketBatch, PacketFlags},
        solana_runtime::genesis_utils,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction,
            hash::Hash,
            message::Message,
            signature::{Keypair, Signer},
            signer::SeedDerivable,
            system_instruction, system_transaction,
            transaction::Transaction,
        },
        solana_vote_program::{
            vote_state::TowerSync, vote_transaction::new_tower_sync_transaction,
        },
        std::error::Error,
    };

    #[test]
    fn transfer_encoding() {
        let from = Keypair::from_seed(&[1; 32]).unwrap();
        let to = Pubkey::new_from_array([2; 32]);
        let amount = 250;
        let transfer =
            system_transaction::transfer(&from, &to, amount, Hash::new_from_array([3; 32]));

        let (recovered_to, recovered_amount) = BundleStorage::extract_transfer(
            &transfer.message.account_keys,
            &solana_system_program::id(),
            &transfer.message.instructions[0],
        )
        .unwrap();
        assert_eq!(recovered_to, &to);
        assert_eq!(recovered_amount, amount);
    }

    #[test]
    fn test_priority_sorting_behavior() {
        let mut priorities = vec![
            (std::cmp::Reverse(100), 1),
            (std::cmp::Reverse(200), 2),
            (std::cmp::Reverse(100), 3),
            (std::cmp::Reverse(50), 4),
        ];
        priorities.sort();
        assert_eq!(
            priorities,
            vec![
                (std::cmp::Reverse(200), 2),
                (std::cmp::Reverse(100), 1),
                (std::cmp::Reverse(100), 3),
                (std::cmp::Reverse(50), 4),
            ]
        );
    }

    #[test]
    fn test_bundle_priority_calculation() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(1_000_000_000);
        genesis_config.fee_rate_governor.lamports_per_signature = 5000;
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.feature_set = Arc::new(FeatureSet::all_enabled());
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        assert!(bank
            .feature_set
            .is_active(&solana_sdk::feature_set::reward_full_priority_fee::id()));

        let blockhash = bank.last_blockhash();

        let payer1 = Keypair::new();
        let payer2 = Keypair::new();
        let payer3 = Keypair::new();
        let dest1 = Keypair::new();
        let dest2 = Keypair::new();
        let dest3 = Keypair::new();
        let dest4 = Keypair::new();
        let tip_account = Keypair::new();

        let tip_accounts = [tip_account.pubkey()]
            .iter()
            .cloned()
            .collect::<HashSet<_>>();

        let create_tx_with_priority_fee = |payer: &Keypair,
                                           dest: &Pubkey,
                                           transfer_amount: u64,
                                           priority_fee_per_cu: u64,
                                           compute_units: u32|
         -> Transaction {
            let mut instructions = vec![];
            if compute_units > 0 {
                instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                    compute_units,
                ));
            }
            if priority_fee_per_cu > 0 {
                instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                    priority_fee_per_cu,
                ));
            }
            instructions.push(system_instruction::transfer(
                &payer.pubkey(),
                dest,
                transfer_amount,
            ));

            let message = Message::new(&instructions, Some(&payer.pubkey()));
            Transaction::new(&[payer], message, blockhash)
        };

        let create_bundles_from_transactions =
            |transactions: &[Transaction]| -> ImmutableDeserializedBundle {
                ImmutableDeserializedBundle::new(
                    &mut PacketBundle {
                        batch: PacketBatch::new(
                            transactions
                                .iter()
                                .map(|tx| Packet::from_data(None, tx).unwrap())
                                .collect::<Vec<_>>(),
                        ),
                        bundle_id: format!("test_bundle_{}", rand::random::<u32>()),
                    },
                    None,
                    &Ok,
                )
                .unwrap()
            };

        // Bundle 1: 1 transaction with low priority fee
        let bundle1_txs = vec![create_tx_with_priority_fee(
            &payer1,
            &dest1.pubkey(),
            1_000,
            100,
            50_000,
        )];

        // Bundle 2: 1 transaction with high priority fee
        let bundle2_txs = vec![create_tx_with_priority_fee(
            &payer2,
            &dest2.pubkey(),
            2_000,
            5000,
            40_000,
        )];

        // Bundle 3: 3 transactions with medium priority fees + one tip transaction
        let bundle3_txs = vec![
            create_tx_with_priority_fee(&payer1, &dest3.pubkey(), 500, 1000, 30_000),
            system_transaction::transfer(&payer2, &tip_account.pubkey(), 50_000, blockhash),
            create_tx_with_priority_fee(&payer3, &dest4.pubkey(), 1_500, 1200, 35_000),
        ];

        // Create immutable bundles
        let immutable_bundle1 = create_bundles_from_transactions(&bundle1_txs);
        let immutable_bundle2 = create_bundles_from_transactions(&bundle2_txs);
        let immutable_bundle3 = create_bundles_from_transactions(&bundle3_txs);

        // Create sanitized bundles (similar to drain_and_sanitize_bundles)
        let mut error_metrics = TransactionErrorMetrics::default();
        let move_precompile_verification_to_svm = bank
            .feature_set
            .is_active(&move_precompile_verification_to_svm::id());

        let sanitized_bundle1 = immutable_bundle1
            .build_sanitized_bundle(
                &bank,
                &HashSet::default(),
                &mut error_metrics,
                move_precompile_verification_to_svm,
            )
            .expect("Bundle 1 should sanitize successfully");

        let sanitized_bundle2 = immutable_bundle2
            .build_sanitized_bundle(
                &bank,
                &HashSet::default(),
                &mut error_metrics,
                move_precompile_verification_to_svm,
            )
            .expect("Bundle 2 should sanitize successfully");

        let sanitized_bundle3 = immutable_bundle3
            .build_sanitized_bundle(
                &bank,
                &HashSet::default(),
                &mut error_metrics,
                move_precompile_verification_to_svm,
            )
            .expect("Bundle 3 should sanitize successfully");

        let mut priority_counter = 0;
        let bundle1_priority_key = BundleStorage::calculate_bundle_priority(
            &immutable_bundle1,
            &sanitized_bundle1,
            &bank,
            &mut priority_counter,
            &tip_accounts,
        );

        let bundle2_priority_key = BundleStorage::calculate_bundle_priority(
            &immutable_bundle2,
            &sanitized_bundle2,
            &bank,
            &mut priority_counter,
            &tip_accounts,
        );

        let bundle3_priority_key = BundleStorage::calculate_bundle_priority(
            &immutable_bundle3,
            &sanitized_bundle3,
            &bank,
            &mut priority_counter,
            &tip_accounts,
        );

        let mut bundles_with_priority = vec![
            (bundle1_priority_key, "Bundle 1"),
            (bundle2_priority_key, "Bundle 2"),
            (bundle3_priority_key, "Bundle 3"),
        ];

        bundles_with_priority.sort_by_key(|(priority_key, _)| *priority_key);

        assert_eq!(bundles_with_priority[0].1, "Bundle 3");
        assert_eq!(bundles_with_priority[1].1, "Bundle 2");
        assert_eq!(bundles_with_priority[2].1, "Bundle 1");
    }

    #[test]
    fn test_filter_processed_packets() {
        let retryable_indexes = [0, 1, 2, 3];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert!(non_retryable_indexes.is_empty());

        let retryable_indexes = [0, 1, 2, 3, 5];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(4, 5)]);

        let retryable_indexes = [1, 2, 3];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1)]);

        let retryable_indexes = [1, 2, 3, 5];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5)]);

        let retryable_indexes = [1, 2, 3, 5, 8];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5), (6, 8)]);

        let retryable_indexes = [1, 2, 3, 5, 8, 8];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5), (6, 8)]);
    }

    #[test]
    fn test_filter_and_forward_with_account_limits() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10);
        let (current_bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let simple_transactions: Vec<Transaction> = (0..256)
            .map(|_id| {
                // packets are deserialized upon receiving, failed packets will not be
                // forwarded; Therefore we need to create real packets here.
                let key1 = Keypair::new();
                system_transaction::transfer(
                    &mint_keypair,
                    &key1.pubkey(),
                    genesis_config.rent.minimum_balance(0),
                    genesis_config.hash(),
                )
            })
            .collect_vec();

        let mut packets: Vec<DeserializedPacket> = simple_transactions
            .iter()
            .enumerate()
            .map(|(packets_id, transaction)| {
                let mut p = Packet::from_data(None, transaction).unwrap();
                p.meta_mut().port = packets_id as u16;
                DeserializedPacket::new(p).unwrap()
            })
            .collect_vec();

        // all packets are forwarded
        {
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let mut transaction_storage = UnprocessedTransactionStorage::new_transaction_storage(
                buffered_packet_batches,
                ThreadType::Transactions,
            );
            let mut forward_packet_batches_by_accounts =
                ForwardPacketBatchesByAccounts::new_with_default_batch_limits();

            let FilterForwardingResults {
                total_forwardable_packets,
                ..
            } = transaction_storage.filter_forwardable_packets_and_add_batches(
                current_bank.clone(),
                &mut forward_packet_batches_by_accounts,
            );
            assert_eq!(total_forwardable_packets, 256);

            // packets in a batch are forwarded in arbitrary order; verify the ports match after
            // sorting
            let expected_ports: Vec<_> = (0..256).collect();
            let mut forwarded_ports: Vec<_> = forward_packet_batches_by_accounts
                .iter_batches()
                .flat_map(|batch| batch.get_forwardable_packets().map(|p| p.meta().port))
                .collect();
            forwarded_ports.sort_unstable();
            assert_eq!(expected_ports, forwarded_ports);
        }

        // some packets are forwarded
        {
            let num_already_forwarded = 16;
            for packet in &mut packets[0..num_already_forwarded] {
                packet.forwarded = true;
            }
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let mut transaction_storage = UnprocessedTransactionStorage::new_transaction_storage(
                buffered_packet_batches,
                ThreadType::Transactions,
            );
            let mut forward_packet_batches_by_accounts =
                ForwardPacketBatchesByAccounts::new_with_default_batch_limits();
            let FilterForwardingResults {
                total_forwardable_packets,
                ..
            } = transaction_storage.filter_forwardable_packets_and_add_batches(
                current_bank.clone(),
                &mut forward_packet_batches_by_accounts,
            );
            assert_eq!(
                total_forwardable_packets,
                packets.len() - num_already_forwarded
            );
        }

        // some packets are invalid (already processed)
        {
            let num_already_processed = 16;
            for tx in &simple_transactions[0..num_already_processed] {
                assert_eq!(current_bank.process_transaction(tx), Ok(()));
            }
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let mut transaction_storage = UnprocessedTransactionStorage::new_transaction_storage(
                buffered_packet_batches,
                ThreadType::Transactions,
            );
            let mut forward_packet_batches_by_accounts =
                ForwardPacketBatchesByAccounts::new_with_default_batch_limits();
            let FilterForwardingResults {
                total_forwardable_packets,
                ..
            } = transaction_storage.filter_forwardable_packets_and_add_batches(
                current_bank,
                &mut forward_packet_batches_by_accounts,
            );
            assert_eq!(
                total_forwardable_packets,
                packets.len() - num_already_processed
            );
        }
    }

    #[test]
    fn test_unprocessed_transaction_storage_insert() -> Result<(), Box<dyn Error>> {
        let keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let pubkey = solana_pubkey::new_rand();

        let small_transfer = Packet::from_data(
            None,
            system_transaction::transfer(&keypair, &pubkey, 1, Hash::new_unique()),
        )?;
        let mut vote = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::default(),
                Hash::new_unique(),
                &keypair,
                &vote_keypair,
                &vote_keypair,
                None,
            ),
        )?;
        vote.meta_mut().flags.set(PacketFlags::SIMPLE_VOTE_TX, true);
        let big_transfer = Packet::from_data(
            None,
            system_transaction::transfer(&keypair, &pubkey, 1000000, Hash::new_unique()),
        )?;

        for thread_type in [
            ThreadType::Transactions,
            ThreadType::Voting(VoteSource::Gossip),
            ThreadType::Voting(VoteSource::Tpu),
        ] {
            let mut transaction_storage = UnprocessedTransactionStorage::new_transaction_storage(
                UnprocessedPacketBatches::with_capacity(100),
                thread_type,
            );
            transaction_storage.insert_batch(vec![
                ImmutableDeserializedPacket::new(small_transfer.clone())?,
                ImmutableDeserializedPacket::new(vote.clone())?,
                ImmutableDeserializedPacket::new(big_transfer.clone())?,
            ]);
            let deserialized_packets = transaction_storage
                .iter()
                .map(|packet| packet.immutable_section().original_packet().clone())
                .collect_vec();
            assert_eq!(3, deserialized_packets.len());
            assert!(deserialized_packets.contains(&small_transfer));
            assert!(deserialized_packets.contains(&vote));
            assert!(deserialized_packets.contains(&big_transfer));
        }

        for (vote_source, staked) in iproduct!(
            [VoteSource::Gossip, VoteSource::Tpu].into_iter(),
            [true, false].into_iter()
        ) {
            let staked_keys = if staked {
                vec![vote_keypair.pubkey()]
            } else {
                vec![]
            };
            let latest_unprocessed_votes = LatestUnprocessedVotes::new_for_tests(&staked_keys);
            let mut transaction_storage = UnprocessedTransactionStorage::new_vote_storage(
                Arc::new(latest_unprocessed_votes),
                vote_source,
            );
            transaction_storage.insert_batch(vec![
                ImmutableDeserializedPacket::new(small_transfer.clone())?,
                ImmutableDeserializedPacket::new(vote.clone())?,
                ImmutableDeserializedPacket::new(big_transfer.clone())?,
            ]);
            assert_eq!(if staked { 1 } else { 0 }, transaction_storage.len());
        }
        Ok(())
    }

    #[test]
    fn test_process_packets_retryable_indexes_reinserted() -> Result<(), Box<dyn Error>> {
        let node_keypair = Keypair::new();
        let genesis_config =
            genesis_utils::create_genesis_config_with_leader(100, &node_keypair.pubkey(), 200)
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let vote_keypair = Keypair::new();
        let mut vote = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::default(),
                Hash::new_unique(),
                &node_keypair,
                &vote_keypair,
                &vote_keypair,
                None,
            ),
        )?;
        vote.meta_mut().flags.set(PacketFlags::SIMPLE_VOTE_TX, true);

        let latest_unprocessed_votes =
            LatestUnprocessedVotes::new_for_tests(&[vote_keypair.pubkey()]);
        let mut transaction_storage = UnprocessedTransactionStorage::new_vote_storage(
            Arc::new(latest_unprocessed_votes),
            VoteSource::Tpu,
        );

        transaction_storage.insert_batch(vec![ImmutableDeserializedPacket::new(vote.clone())?]);
        assert_eq!(1, transaction_storage.len());

        // When processing packets, return all packets as retryable so that they
        // are reinserted into storage
        let _ = transaction_storage.process_packets(
            bank.clone(),
            &BankingStageStats::default(),
            &mut LeaderSlotMetricsTracker::new(0),
            |packets, _payload| {
                // Return all packets indexes as retryable
                Some(
                    packets
                        .iter()
                        .enumerate()
                        .map(|(index, _packet)| index)
                        .collect_vec(),
                )
            },
            &HashSet::default(),
        );

        // All packets should remain in the transaction storage
        assert_eq!(1, transaction_storage.len());
        Ok(())
    }

    #[test]
    fn test_prepare_packets_to_forward() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10);

        let simple_transactions: Vec<Transaction> = (0..256)
            .map(|_id| {
                // packets are deserialized upon receiving, failed packets will not be
                // forwarded; Therefore we need to create real packets here.
                let key1 = Keypair::new();
                system_transaction::transfer(
                    &mint_keypair,
                    &key1.pubkey(),
                    genesis_config.rent.minimum_balance(0),
                    genesis_config.hash(),
                )
            })
            .collect_vec();

        let mut packets: Vec<DeserializedPacket> = simple_transactions
            .iter()
            .enumerate()
            .map(|(packets_id, transaction)| {
                let mut p = Packet::from_data(None, transaction).unwrap();
                p.meta_mut().port = packets_id as u16;
                DeserializedPacket::new(p).unwrap()
            })
            .collect_vec();

        // test preparing buffered packets for forwarding
        let test_prepareing_buffered_packets_for_forwarding =
            |buffered_packet_batches: UnprocessedPacketBatches| -> usize {
                let mut total_packets_to_forward: usize = 0;

                let mut unprocessed_transactions = ThreadLocalUnprocessedPackets {
                    unprocessed_packet_batches: buffered_packet_batches,
                    thread_type: ThreadType::Transactions,
                };

                let mut original_priority_queue = unprocessed_transactions.take_priority_queue();
                let _ = original_priority_queue
                    .drain_desc()
                    .chunks(128usize)
                    .into_iter()
                    .flat_map(|packets_to_process| {
                        let (_, packets_to_forward) =
                            unprocessed_transactions.prepare_packets_to_forward(packets_to_process);
                        total_packets_to_forward += packets_to_forward.len();
                        packets_to_forward
                    })
                    .collect::<MinMaxHeap<Arc<ImmutableDeserializedPacket>>>();
                total_packets_to_forward
            };

        {
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let total_packets_to_forward =
                test_prepareing_buffered_packets_for_forwarding(buffered_packet_batches);
            assert_eq!(total_packets_to_forward, 256);
        }

        // some packets are forwarded
        {
            let num_already_forwarded = 16;
            for packet in &mut packets[0..num_already_forwarded] {
                packet.forwarded = true;
            }
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let total_packets_to_forward =
                test_prepareing_buffered_packets_for_forwarding(buffered_packet_batches);
            assert_eq!(total_packets_to_forward, 256 - num_already_forwarded);
        }

        // all packets are forwarded
        {
            for packet in &mut packets {
                packet.forwarded = true;
            }
            let buffered_packet_batches: UnprocessedPacketBatches =
                UnprocessedPacketBatches::from_iter(packets.clone(), packets.len());
            let total_packets_to_forward =
                test_prepareing_buffered_packets_for_forwarding(buffered_packet_batches);
            assert_eq!(total_packets_to_forward, 0);
        }
    }
}
