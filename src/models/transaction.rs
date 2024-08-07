#[cfg(not(feature = "hive"))]
use crate::eth_provider::starknet::kakarot_core::MAX_FELTS_IN_CALLDATA;
use crate::{
    eth_provider::{
        error::{EthApiError, SignatureError, TransactionError},
        starknet::kakarot_core::{get_white_listed_eip_155_transaction_hashes, ETH_SEND_TRANSACTION, KAKAROT_ADDRESS},
        utils::split_u256,
    },
    tracing::builder::TRACING_BLOCK_GAS_LIMIT,
};
use alloy_rlp::Encodable;
use reth_primitives::{Transaction, TransactionSigned};
use reth_rpc_types::Header;
use starknet::core::types::Felt;

/// Validates the signed ethereum transaction.
/// The validation checks the following:
/// - The transaction gas limit is lower than the tracing block gas limit.
/// - The transaction chain id (if any) is the same as the one provided.
/// - The transaction hash is whitelisted for pre EIP-155 transactions.
/// - The transaction signature can be recovered.
/// - The transaction base fee is lower than the max fee per gas.
/// - The transaction max priority fee is lower than the max fee per gas.
/// - The transaction gas limit is lower than the block's gas limit.
///
/// # Errors
///
/// Returns an error if the transaction is invalid.
pub(crate) fn validate_transaction(
    transaction_signed: &TransactionSigned,
    chain_id: u64,
    previous_block_header: &Header,
) -> Result<(), EthApiError> {
    // If the transaction gas limit is higher than the tracing
    // block gas limit, prevent the transaction from being sent
    // (it will revert anyway on the Starknet side). This assures
    // that all transactions are traceable.
    if transaction_signed.gas_limit() > TRACING_BLOCK_GAS_LIMIT {
        return Err(TransactionError::GasOverflow.into());
    }

    // Recover the signer from the transaction
    let _ = transaction_signed.recover_signer().ok_or(SignatureError::Recovery)?;

    // Assert the chain is correct
    let maybe_chain_id = transaction_signed.chain_id();
    if !maybe_chain_id.map_or(true, |c| c == chain_id) {
        return Err(TransactionError::InvalidChainId.into());
    }

    // If the transaction is a pre EIP-155 transaction, check if hash is whitelisted
    if maybe_chain_id.is_none() && !get_white_listed_eip_155_transaction_hashes().contains(&transaction_signed.hash) {
        return Err(TransactionError::InvalidTransactionType.into());
    }

    let base_fee = previous_block_header.base_fee_per_gas.unwrap_or_default();
    let max_fee_per_gas = transaction_signed.max_fee_per_gas();

    // Check if the base fee is lower than the max fee per gas
    if base_fee > max_fee_per_gas {
        return Err(TransactionError::FeeCapTooLow(max_fee_per_gas, base_fee).into());
    }

    let max_priority_fee_per_gas = transaction_signed.max_priority_fee_per_gas().unwrap_or_default();

    // Check if the max priority fee is lower than the max fee per gas
    if max_priority_fee_per_gas > max_fee_per_gas {
        return Err(TransactionError::TipAboveFeeCap(max_fee_per_gas, max_priority_fee_per_gas).into());
    }

    let transaction_gas_limit = transaction_signed.gas_limit().into();
    let block_gas_limit = previous_block_header.gas_limit;

    // Check if the transaction gas limit is lower than the block's gas limit
    if transaction_gas_limit > block_gas_limit {
        return Err(TransactionError::ExceedsBlockGasLimit(transaction_gas_limit, block_gas_limit).into());
    }

    Ok(())
}

/// Returns the transaction's signature as a [`Vec<Felt>`].
/// Fields r and s are split into two 16-bytes chunks both converted
/// to [`Felt`].
pub(crate) fn transaction_signature_to_field_elements(transaction_signed: &TransactionSigned) -> Vec<Felt> {
    let transaction_signature = transaction_signed.signature();

    let mut signature = Vec::with_capacity(5);
    signature.extend_from_slice(&split_u256(transaction_signature.r));
    signature.extend_from_slice(&split_u256(transaction_signature.s));

    // Push the last element of the signature
    // In case of a Legacy Transaction, it is v := {0, 1} + chain_id * 2 + 35
    // or {0, 1} + 27 for pre EIP-155 transactions.
    // Else, it is odd_y_parity
    if let Transaction::Legacy(_) = transaction_signed.transaction {
        let chain_id = transaction_signed.chain_id();
        signature.push(transaction_signature.v(chain_id).into());
    } else {
        signature.push(u64::from(transaction_signature.odd_y_parity).into());
    }

    signature
}

/// Returns the transaction's data RLP encoded without the signature as a [`Vec<Felt>`].
/// The data is appended to the Starknet invoke transaction calldata.
///
/// # Example
///
/// For Legacy Transactions: rlp([nonce, `gas_price`, `gas_limit`, to, value, data, `chain_id`, 0, 0])
/// is then converted to a [`Vec<Felt>`], packing the data in 31-byte chunks.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn transaction_data_to_starknet_calldata(
    transaction_signed: &TransactionSigned,
    retries: u8,
) -> Result<Vec<Felt>, EthApiError> {
    let mut signed_data = Vec::with_capacity(transaction_signed.transaction.length());
    transaction_signed.transaction.encode_without_signature(&mut signed_data);

    // Pack the calldata in 31-byte chunks
    let mut signed_data: Vec<Felt> = std::iter::once(Felt::from(signed_data.len()))
        .chain(signed_data.chunks(31).map(Felt::from_bytes_be_slice))
        .collect();

    // Prepare the calldata for the Starknet invoke transaction
    let capacity = 6 + signed_data.len();

    // Check if call data is too large
    #[cfg(not(feature = "hive"))]
    if capacity > *MAX_FELTS_IN_CALLDATA {
        return Err(EthApiError::CalldataExceededLimit(*MAX_FELTS_IN_CALLDATA, capacity));
    }

    let mut calldata = Vec::with_capacity(capacity);

    // assert that the selector < Felt::MAX - retries
    assert!(*ETH_SEND_TRANSACTION < Felt::MAX - Felt::from(retries));
    let selector = *ETH_SEND_TRANSACTION + Felt::from(retries);

    // Retries are used to alter the transaction hash in order to avoid the
    // `DuplicateTx` error from the Starknet gateway, encountered whenever
    // a transaction with the same hash is sent multiple times.
    // We add the retries to the selector in the calldata, since the selector
    // is not used by the EOA contract during the transaction execution.
    calldata.append(&mut vec![
        Felt::ONE,                // call array length
        *KAKAROT_ADDRESS,         // contract address
        selector,                 // selector + retries
        Felt::ZERO,               // data offset
        signed_data.len().into(), // data length
        signed_data.len().into(), // calldata length
    ]);
    calldata.append(&mut signed_data);

    Ok(calldata)
}
