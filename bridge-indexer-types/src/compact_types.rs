use crate::stream_types::{
    AccountId, Balance, CryptoHash, ExecutionStatusView, ReceiptView, SignedTransactionView,
};

#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactExecutionOutcomeWithIdView<Outcome = CompactExecutionOutcomeView> {
    pub block_hash: CryptoHash,
    pub id: CryptoHash,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompactIndexerExecutionOutcomeWithOptionalReceipt<
    ExecutionOutcome = CompactExecutionOutcomeWithIdView,
    ReceiptViewT = ReceiptView,
> {
    pub execution_outcome: ExecutionOutcome,
    pub receipt: Option<ReceiptViewT>,
}

#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactExecutionOutcomeView<AccountIdT = AccountId, Status = ExecutionStatusView> {
    pub logs: Vec<String>,
    pub receipt_ids: Vec<CryptoHash>,
    pub gas_burnt: u64,
    pub tokens_burnt: Balance,
    pub executor_id: AccountIdT,
    pub status: Status,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompactIndexerExecutionOutcomeWithReceipt<
    ExecutionOutcome = CompactExecutionOutcomeWithIdView,
    ReceiptViewT = ReceiptView,
> {
    pub execution_outcome: ExecutionOutcome,
    pub receipt: ReceiptViewT,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompactIndexerTransactionWithOutcome<
    Transaction = SignedTransactionView,
    Outcome = CompactIndexerExecutionOutcomeWithOptionalReceipt,
> {
    pub transaction: Transaction,
    pub outcome: Outcome,
}
