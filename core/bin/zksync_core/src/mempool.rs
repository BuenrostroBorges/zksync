//! Mempool is simple in memory buffer for transactions.
//!
//! Its role is to:
//! 1) Accept transactions from api, check signatures and basic nonce correctness(nonce not too small).
//! To do nonce correctness check mempool stores mapping `AccountAddress -> Nonce`, this mapping is updated
//! when new block is committed.
//! 2) When polled return vector of the transactions in the queue.
//!
//! Mempool is not persisted on disc, all transactions will be lost on node shutdown.
//!
//! Communication channel with other actors:
//! Mempool does not push information to other actors, only accepts requests. (see `MempoolRequest`)
//!
//! Communication with db:
//! on restart mempool restores nonces of the accounts that are stored in the account tree.

// Built-in deps
use std::collections::{HashMap, VecDeque};
// External uses
use futures::{
    channel::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
    SinkExt, StreamExt,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinHandle;
// Workspace uses
use zksync_storage::ConnectionPool;
use zksync_types::{
    mempool::{SignedTxVariant, SignedTxsBatch},
    tx::TxEthSignature,
    AccountId, AccountUpdate, AccountUpdates, Address, Nonce, PriorityOp, SignedZkSyncTx,
    TransferOp, TransferToNewOp, ZkSyncTx,
};
// Local uses
use crate::eth_watch::EthWatchRequest;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use zksync_config::ConfigurationOptions;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Error)]
pub enum TxAddError {
    #[error("Tx nonce is too low.")]
    NonceMismatch,

    #[error("Tx is incorrect")]
    IncorrectTx,

    #[error("Transaction fee is too low")]
    TxFeeTooLow,

    #[error("Transactions batch summary fee is too low")]
    TxBatchFeeTooLow,

    #[error("EIP1271 signature could not be verified")]
    EIP1271SignatureVerificationFail,

    #[error("MissingEthSignature")]
    MissingEthSignature,

    #[error("Eth signature is incorrect")]
    IncorrectEthSignature,

    #[error("Change pubkey tx is not authorized onchain")]
    ChangePkNotAuthorized,

    #[error("Internal error")]
    Other,

    #[error("Database unavailable")]
    DbError,

    #[error("Transaction batch is empty")]
    EmptyBatch,

    #[error("Batch will not fit in any of supported block sizes")]
    BatchTooBig,

    #[error("The number of withdrawals in the batch is too big")]
    BatchWithdrawalsOverload,
}

#[derive(Clone, Debug, Default)]
pub struct ProposedBlock {
    pub priority_ops: Vec<PriorityOp>,
    pub txs: Vec<SignedTxVariant>,
}

impl ProposedBlock {
    pub fn is_empty(&self) -> bool {
        self.priority_ops.is_empty() && self.txs.is_empty()
    }
}

#[derive(Debug)]
pub struct GetBlockRequest {
    pub last_priority_op_number: u64,
    pub response_sender: oneshot::Sender<ProposedBlock>,
}

#[derive(Debug)]
pub enum MempoolTransactionRequest {
    /// Add new transaction to mempool, transaction should be previously checked
    /// for correctness (including its Ethereum and ZKSync signatures).
    /// oneshot is used to receive tx add result.
    NewTx(Box<SignedZkSyncTx>, oneshot::Sender<Result<(), TxAddError>>),
    /// Add a new batch of transactions to the mempool. All transactions in batch must
    /// be either executed successfully, or otherwise fail all together.
    /// Invariants for each individual transaction in the batch are the same as in
    /// `NewTx` variant of this enum.
    NewTxsBatch(
        Vec<SignedZkSyncTx>,
        Option<TxEthSignature>,
        oneshot::Sender<Result<(), TxAddError>>,
    ),
}

#[derive(Debug)]
pub enum MempoolBlocksRequest {
    /// When block is committed, nonces of the account tree should be updated too.
    UpdateNonces(AccountUpdates),
    /// Get transactions from the mempool.
    GetBlock(GetBlockRequest),
}

struct MempoolState {
    // account and last committed nonce
    account_nonces: HashMap<Address, Nonce>,
    account_ids: HashMap<AccountId, Address>,
    ready_txs: VecDeque<SignedTxVariant>,
}

impl MempoolState {
    fn chunks_for_tx(&self, tx: &ZkSyncTx) -> usize {
        match tx {
            ZkSyncTx::Transfer(tx) => {
                if self.account_nonces.contains_key(&tx.to) {
                    TransferOp::CHUNKS
                } else {
                    TransferToNewOp::CHUNKS
                }
            }
            _ => tx.min_chunks(),
        }
    }

    fn chunks_for_batch(&self, batch: &SignedTxsBatch) -> usize {
        batch.txs.iter().map(|tx| self.chunks_for_tx(&tx.tx)).sum()
    }

    fn required_chunks(&self, element: &SignedTxVariant) -> usize {
        match element {
            SignedTxVariant::Tx(tx) => self.chunks_for_tx(&tx.tx),
            SignedTxVariant::Batch(batch) => self.chunks_for_batch(batch),
        }
    }

    async fn restore_from_db(db_pool: &ConnectionPool) -> Self {
        let mut storage = db_pool.access_storage().await.expect("mempool db restore");
        let mut transaction = storage
            .start_transaction()
            .await
            .expect("mempool db transaction");

        let (_, accounts) = transaction
            .chain()
            .state_schema()
            .load_committed_state(None)
            .await
            .expect("mempool account state load");

        let mut account_ids = HashMap::new();
        let mut account_nonces = HashMap::new();

        for (id, account) in accounts {
            account_ids.insert(id, account.address);
            account_nonces.insert(account.address, account.nonce);
        }

        // Remove any possible duplicates of already executed transactions
        // from the database.
        transaction
            .chain()
            .mempool_schema()
            .collect_garbage()
            .await
            .expect("Collecting garbage in the mempool schema failed");

        // Load transactions that were not yet processed and are awaiting in the
        // mempool.
        let ready_txs: VecDeque<_> = transaction
            .chain()
            .mempool_schema()
            .load_txs()
            .await
            .expect("Attempt to restore mempool txs from DB failed");

        transaction
            .commit()
            .await
            .expect("mempool db transaction commit");

        log::info!(
            "{} transactions were restored from the persistent mempool storage",
            ready_txs.len()
        );

        Self {
            account_nonces,
            account_ids,
            ready_txs,
        }
    }

    fn nonce(&self, address: &Address) -> Nonce {
        *self.account_nonces.get(address).unwrap_or(&0)
    }

    fn add_tx(&mut self, tx: SignedZkSyncTx) -> Result<(), TxAddError> {
        // Correctness should be checked by `signature_checker`, thus
        // `tx.check_correctness()` is not invoked here.

        if tx.nonce() >= self.nonce(&tx.account()) {
            self.ready_txs.push_back(tx.into());
            Ok(())
        } else {
            Err(TxAddError::NonceMismatch)
        }
    }

    fn add_batch(&mut self, batch: SignedTxsBatch) -> Result<(), TxAddError> {
        assert_ne!(batch.batch_id, 0, "Batch ID was not set");

        for tx in batch.txs.iter() {
            if tx.nonce() < self.nonce(&tx.account()) {
                return Err(TxAddError::NonceMismatch);
            }
        }

        self.ready_txs.push_back(SignedTxVariant::Batch(batch));

        Ok(())
    }
}

struct MempoolBlocks {
    mempool_state: Arc<Mutex<MempoolState>>,
    requests: mpsc::Receiver<MempoolBlocksRequest>,
    eth_watch_req: mpsc::Sender<EthWatchRequest>,
    max_block_size_chunks: usize,
}

impl MempoolBlocks {
    async fn propose_new_block(&mut self, current_unprocessed_priority_op: u64) -> ProposedBlock {
        let start = std::time::Instant::now();
        let (chunks_left, priority_ops) = self
            .select_priority_ops(current_unprocessed_priority_op)
            .await;
        let (_chunks_left, txs) = self.prepare_tx_for_block(chunks_left).await;

        log::trace!("Proposed priority ops for block: {:#?}", priority_ops);
        log::trace!("Proposed txs for block: {:#?}", txs);
        metrics::histogram!("mempool.propose_new_block", start.elapsed());
        ProposedBlock { priority_ops, txs }
    }

    /// Returns: chunks left from max amount of chunks, ops selected
    async fn select_priority_ops(
        &self,
        current_unprocessed_priority_op: u64,
    ) -> (usize, Vec<PriorityOp>) {
        let eth_watch_resp = oneshot::channel();
        self.eth_watch_req
            .clone()
            .send(EthWatchRequest::GetPriorityQueueOps {
                op_start_id: current_unprocessed_priority_op,
                max_chunks: self.max_block_size_chunks,
                resp: eth_watch_resp.0,
            })
            .await
            .expect("ETH watch req receiver dropped");

        let priority_ops = eth_watch_resp.1.await.expect("Err response from eth watch");

        (
            self.max_block_size_chunks
                - priority_ops
                    .iter()
                    .map(|op| op.data.chunks())
                    .sum::<usize>(),
            priority_ops,
        )
    }

    async fn prepare_tx_for_block(
        &mut self,
        mut chunks_left: usize,
    ) -> (usize, Vec<SignedTxVariant>) {
        let mut txs_for_commit = Vec::new();

        let mut mempool = self.mempool_state.lock().await;
        while let Some(tx) = mempool.ready_txs.pop_front() {
            let chunks_for_tx = mempool.required_chunks(&tx);
            if chunks_left >= chunks_for_tx {
                txs_for_commit.push(tx);
                chunks_left -= chunks_for_tx;
            } else {
                // Push the taken tx back, it does not fit.
                mempool.ready_txs.push_front(tx);
                break;
            }
        }

        (chunks_left, txs_for_commit)
    }

    async fn run(mut self) {
        while let Some(request) = self.requests.next().await {
            match request {
                MempoolBlocksRequest::GetBlock(block) => {
                    // Generate proposed block.
                    let proposed_block =
                        self.propose_new_block(block.last_priority_op_number).await;

                    // Send the proposed block to the request initiator.
                    block
                        .response_sender
                        .send(proposed_block)
                        .expect("mempool proposed block response send failed");
                }
                MempoolBlocksRequest::UpdateNonces(updates) => {
                    for (id, update) in updates {
                        match update {
                            AccountUpdate::Create { address, nonce } => {
                                let mut mempool = self.mempool_state.lock().await;
                                mempool.account_ids.insert(id, address);
                                mempool.account_nonces.insert(address, nonce);
                            }
                            AccountUpdate::Delete { address, .. } => {
                                let mut mempool = self.mempool_state.lock().await;
                                mempool.account_ids.remove(&id);
                                mempool.account_nonces.remove(&address);
                            }
                            AccountUpdate::UpdateBalance { new_nonce, .. } => {
                                if let Some(address) =
                                    self.mempool_state.lock().await.account_ids.get(&id)
                                {
                                    if let Some(nonce) = self
                                        .mempool_state
                                        .lock()
                                        .await
                                        .account_nonces
                                        .get_mut(address)
                                    {
                                        *nonce = new_nonce;
                                    }
                                }
                            }
                            AccountUpdate::ChangePubKeyHash { new_nonce, .. } => {
                                if let Some(address) =
                                    self.mempool_state.lock().await.account_ids.get(&id)
                                {
                                    if let Some(nonce) = self
                                        .mempool_state
                                        .lock()
                                        .await
                                        .account_nonces
                                        .get_mut(address)
                                    {
                                        *nonce = new_nonce;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

struct MempoolTransactionsHandler {
    db_pool: ConnectionPool,
    mempool_state: Arc<Mutex<MempoolState>>,
    requests: mpsc::Receiver<MempoolTransactionRequest>,
    max_block_size_chunks: usize,
    max_number_of_withdrawals_per_block: usize,
}

impl Balanced<MempoolTransactionRequest> for MempoolTransactionsHandler {
    fn clone_with_receiver(&self, receiver: Receiver<MempoolTransactionRequest>) -> Self {
        Self {
            db_pool: self.db_pool.clone(),
            mempool_state: self.mempool_state.clone(),
            requests: receiver,
            max_block_size_chunks: self.max_block_size_chunks,
            max_number_of_withdrawals_per_block: self.max_number_of_withdrawals_per_block,
        }
    }
}

impl MempoolTransactionsHandler {
    async fn add_tx(&mut self, tx: SignedZkSyncTx) -> Result<(), TxAddError> {
        let mut storage = self.db_pool.access_storage().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;

        let mut transaction = storage.start_transaction().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;
        transaction
            .chain()
            .mempool_schema()
            .insert_tx(&tx)
            .await
            .map_err(|err| {
                log::warn!("Mempool storage access error: {}", err);
                TxAddError::DbError
            })?;

        transaction.commit().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;

        self.mempool_state.lock().await.add_tx(tx)
    }

    async fn add_batch(
        &mut self,
        txs: Vec<SignedZkSyncTx>,
        eth_signature: Option<TxEthSignature>,
    ) -> Result<(), TxAddError> {
        let mut storage = self.db_pool.access_storage().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;

        let mut batch: SignedTxsBatch = SignedTxsBatch {
            txs: txs.clone(),
            batch_id: 0, // Will be determined after inserting to the database
            eth_signature: eth_signature.clone(),
        };

        if self.mempool_state.lock().await.chunks_for_batch(&batch) > self.max_block_size_chunks {
            return Err(TxAddError::BatchTooBig);
        }

        let mut number_of_withdrawals = 0;
        for tx in txs {
            if tx.tx.is_withdraw() {
                number_of_withdrawals += 1;
            }
        }
        if number_of_withdrawals > self.max_number_of_withdrawals_per_block {
            return Err(TxAddError::BatchWithdrawalsOverload);
        }

        let mut transaction = storage.start_transaction().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;
        let batch_id = transaction
            .chain()
            .mempool_schema()
            .insert_batch(&batch.txs, eth_signature)
            .await
            .map_err(|err| {
                log::warn!("Mempool storage access error: {}", err);
                TxAddError::DbError
            })?;
        transaction.commit().await.map_err(|err| {
            log::warn!("Mempool storage access error: {}", err);
            TxAddError::DbError
        })?;

        batch.batch_id = batch_id;

        self.mempool_state.lock().await.add_batch(batch)
    }

    async fn run(mut self) {
        while let Some(request) = self.requests.next().await {
            match request {
                MempoolTransactionRequest::NewTx(tx, resp) => {
                    let tx_add_result = self.add_tx(*tx).await;
                    resp.send(tx_add_result).unwrap_or_default();
                }
                MempoolTransactionRequest::NewTxsBatch(txs, eth_signature, resp) => {
                    let tx_add_result = self.add_batch(txs, eth_signature).await;
                    resp.send(tx_add_result).unwrap_or_default();
                }
            }
        }
    }
}

#[must_use]
pub fn run_mempool_tasks(
    db_pool: ConnectionPool,
    tx_requests: mpsc::Receiver<MempoolTransactionRequest>,
    block_requests: mpsc::Receiver<MempoolBlocksRequest>,
    eth_watch_req: mpsc::Sender<EthWatchRequest>,
    config: &ConfigurationOptions,
    number_of_mempool_transaction_handlers: u8,
    channel_capacity: usize,
) -> JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move {
        let mempool_state = Arc::new(Mutex::new(MempoolState::restore_from_db(&db_pool).await));
        let tmp_channel = mpsc::channel(channel_capacity);
        let max_block_size_chunks = *config
            .available_block_chunk_sizes
            .iter()
            .max()
            .expect("failed to find max block chunks size");

        let mut balancer = Balancer::new(
            MempoolTransactionsHandler {
                db_pool: db_pool.clone(),
                mempool_state: mempool_state.clone(),
                requests: tmp_channel.1,
                max_block_size_chunks,
                max_number_of_withdrawals_per_block: config.max_number_of_withdrawals_per_block,
            },
            tx_requests,
            number_of_mempool_transaction_handlers,
            channel_capacity,
        );

        let mut tasks = vec![];
        while let Some(item) = balancer.balanced_items.pop() {
            tasks.push(tokio::spawn(item.run()));
        }

        tasks.push(tokio::spawn(balancer.run()));
        let blocks_handler = MempoolBlocks {
            mempool_state,
            requests: block_requests,
            eth_watch_req,
            max_block_size_chunks,
        };
        tasks.push(tokio::spawn(blocks_handler.run()));
    })
}

pub struct Balancer<T, REQUESTS> {
    pub balanced_items: Vec<T>,
    channels: Vec<Sender<REQUESTS>>,
    requests: Receiver<REQUESTS>,
}

pub trait Balanced<REQUESTS> {
    fn clone_with_receiver(&self, receiver: Receiver<REQUESTS>) -> Self;
}

impl<T, REQUESTS> Balancer<T, REQUESTS>
where
    T: Balanced<REQUESTS> + Sync + Send + 'static,
{
    pub fn new(
        balanced_item: T,
        requests: Receiver<REQUESTS>,
        number_of_items: u8,
        channel_capacity: usize,
    ) -> Self {
        let mut balanced_items = vec![];
        let mut channels = vec![];

        for _ in 0..number_of_items {
            let (request_sender, request_receiver) = mpsc::channel(channel_capacity);
            channels.push(request_sender);
            balanced_items.push(balanced_item.clone_with_receiver(request_receiver));
        }

        Self {
            balanced_items,
            channels,
            requests,
        }
    }

    pub async fn run(mut self) {
        // It's an obvious way of balancing. Send an equal number of requests to each ticker
        let mut channel_indexes = (0..self.channels.len()).into_iter().cycle();
        // it's the easiest way how to cycle over channels, because cycle required clone trait
        while let Some(request) = self.requests.next().await {
            let channel_index = channel_indexes
                .next()
                .expect("Exactly one channel should exists");
            let start = Instant::now();
            self.channels[channel_index]
                .send(request)
                .await
                .unwrap_or_default();
            metrics::histogram!("ticker.dispatcher.request", start.elapsed());
        }
    }
}
