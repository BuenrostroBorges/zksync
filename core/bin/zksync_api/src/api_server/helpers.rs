//! Helpers collection shared between the different API implementations.

// Built-in uses
use std::collections::HashMap;
use std::time::Instant;

// External uses
use num::BigUint;

// Workspace uses
use zksync_api_types::v02::account::{
    DepositingAccountBalances, DepositingFunds, OngoingDeposit, OngoingDepositsResp,
};
use zksync_storage::StorageProcessor;
use zksync_types::{Address, H256};
use zksync_utils::remove_prefix;

// Local uses
use super::rest::v02::error::Error;
use crate::{
    core_api_client::CoreApiClient, fee_ticker::PriceError, utils::token_db_cache::TokenDBCache,
};

pub fn try_parse_hash(query: &str) -> Result<H256, hex::FromHexError> {
    const HASH_SIZE: usize = 32; // 32 bytes

    let mut slice = [0_u8; HASH_SIZE];

    let tx_hex = remove_prefix(query);
    hex::decode_to_slice(&tx_hex, &mut slice)?;

    Ok(H256::from_slice(&slice))
}

pub(crate) async fn depositing_from_pending_ops(
    storage: &mut StorageProcessor<'_>,
    tokens: &TokenDBCache,
    pending_ops: OngoingDepositsResp,
    confirmations_for_eth_event: u64,
) -> Result<DepositingAccountBalances, Error> {
    let mut balances = HashMap::new();

    for op in pending_ops.deposits {
        let token_symbol = if *op.token_id == 0 {
            "ETH".to_string()
        } else {
            tokens
                .get_token(storage, op.token_id)
                .await
                .map_err(Error::storage)?
                .ok_or_else(|| {
                    Error::from(PriceError::token_not_found("Token not found in storage"))
                })?
                .symbol
        };

        let expected_accept_block = op.received_on_block + confirmations_for_eth_event;

        let balance = balances
            .entry(token_symbol)
            .or_insert_with(DepositingFunds::default);

        balance.amount += BigUint::from(op.amount);

        // `balance.expected_accept_block` should be the greatest block number among
        // all the deposits for a certain token.
        if expected_accept_block > balance.expected_accept_block {
            balance.expected_accept_block = expected_accept_block;
        }
    }

    Ok(DepositingAccountBalances { balances })
}

pub(crate) async fn get_pending_ops(
    core_api_client: &CoreApiClient,
    address: Address,
) -> Result<OngoingDepositsResp, Error> {
    let start = Instant::now();

    let ongoing_ops = core_api_client
        .get_unconfirmed_deposits(address)
        .await
        .map_err(Error::core_api)?;

    // Transform operations into `OngoingDeposit`.
    let deposits: Vec<_> = ongoing_ops.into_iter().map(OngoingDeposit::new).collect();

    metrics::histogram!(
        "api",
        start.elapsed(),
        "type" => "rpc",
        "endpoint_name" => "get_ongoing_deposits"
    );
    Ok(OngoingDepositsResp { deposits })
}

pub async fn get_depositing(
    storage: &mut StorageProcessor<'_>,
    core_api_client: &CoreApiClient,
    tokens: &TokenDBCache,
    address: Address,
    confirmations_for_eth_event: u64,
) -> Result<DepositingAccountBalances, Error> {
    let pending_ops = get_pending_ops(core_api_client, address).await?;
    depositing_from_pending_ops(storage, tokens, pending_ops, confirmations_for_eth_event).await
}
