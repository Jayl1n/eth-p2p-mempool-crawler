use alloy_consensus::{transaction::SignerRecoverable, Transaction as AlloyTransactionTrait};
use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use reth_primitives::{TransactionSigned, TxType};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct TxAnalysisResult {
    pub hash: B256,
    pub tx_type: TxType,
    pub sender: Option<Address>,
    pub receiver: Option<Address>,
    pub value: U256,
    pub gas_limit: u64,
    pub gas_price_or_max_fee: Option<u128>,
    pub max_priority_fee: Option<u128>,
    pub input_len: usize,
    pub first_seen_at: DateTime<Utc>,
    pub is_private: bool,
}

pub fn analyze_transaction(tx_signed: &TransactionSigned) -> TxAnalysisResult {
    let hash = tx_signed.hash();

    let sender = match tx_signed.recover_signer() {
        Ok(addr) => Some(addr),
        Err(e) => {
            warn!(tx_hash=%hash, "Failed to recover sender: {}", e);
            None
        }
    };

    let receiver = tx_signed.to();
    let value = tx_signed.value();
    let gas_limit = tx_signed.gas_limit();
    let input_len = tx_signed.input().len();
    let tx_type = tx_signed.tx_type();

    let (gas_price_or_max_fee, max_priority_fee) = match tx_type {
        TxType::Legacy | TxType::Eip2930 => (tx_signed.gas_price(), None),
        TxType::Eip1559 | TxType::Eip4844 | TxType::Eip7702 => (
            Some(tx_signed.max_fee_per_gas()),
            tx_signed.max_priority_fee_per_gas(),
        ),
    };

    TxAnalysisResult {
        hash: *hash,
        tx_type,
        sender,
        receiver,
        value,
        gas_limit,
        gas_price_or_max_fee,
        max_priority_fee,
        input_len,
        first_seen_at: Utc::now(),
        is_private: false,
    }
}
