use std::collections::HashMap;
// External uses
use jsonrpc_core::{Error, Result};
use num::BigUint;
// Workspace uses
use zksync_types::{
    helpers::closest_packable_fee_amount,
    tx::{TxEthSignature, TxHash},
    Address, Token, TokenLike, TxFeeTypes, ZkSyncTx,
};

// Local uses
use crate::fee_ticker::{BatchFee, Fee, TokenPriceRequestType};
use bigdecimal::BigDecimal;

use super::{error::*, types::*, RpcApp};

impl RpcApp {
    pub async fn _impl_account_info(self, address: Address) -> Result<AccountInfoResp> {
        use std::time::Instant;

        let started = Instant::now();

        let account_state = self.get_account_state(&address).await?;

        let depositing_ops = self.get_ongoing_deposits_impl(address).await?;
        let depositing =
            DepositingAccountBalances::from_pending_ops(depositing_ops, &self.tx_sender.tokens)
                .await?;

        log::trace!(
            "account_info: address {}, total request processing {}ms",
            &address,
            started.elapsed().as_millis()
        );

        Ok(AccountInfoResp {
            address,
            id: account_state.account_id,
            committed: account_state.committed,
            verified: account_state.verified,
            depositing,
        })
    }

    pub async fn _impl_ethop_info(self, serial_id: u32) -> Result<ETHOpInfoResp> {
        let executed_op = self.get_executed_priority_operation(serial_id).await?;
        Ok(if let Some(executed_op) = executed_op {
            let block = self.get_block_info(executed_op.block_number).await?;
            ETHOpInfoResp {
                executed: true,
                block: Some(BlockInfo {
                    block_number: executed_op.block_number,
                    committed: true,
                    verified: block.map(|b| b.verified_at.is_some()).unwrap_or_default(),
                }),
            }
        } else {
            ETHOpInfoResp {
                executed: false,
                block: None,
            }
        })
    }

    pub async fn _impl_get_confirmations_for_eth_op_amount(self) -> Result<u64> {
        Ok(self.confirmations_for_eth_event)
    }

    pub async fn _impl_tx_info(self, tx_hash: TxHash) -> Result<TransactionInfoResp> {
        let stored_receipt = self.get_tx_receipt(tx_hash).await?;
        Ok(if let Some(stored_receipt) = stored_receipt {
            TransactionInfoResp {
                executed: true,
                success: Some(stored_receipt.success),
                fail_reason: stored_receipt.fail_reason,
                block: Some(BlockInfo {
                    block_number: stored_receipt.block_number,
                    committed: true,
                    verified: stored_receipt.verified,
                }),
            }
        } else {
            TransactionInfoResp {
                executed: false,
                success: None,
                fail_reason: None,
                block: None,
            }
        })
    }

    pub async fn _impl_tx_submit(
        self,
        tx: Box<ZkSyncTx>,
        signature: Box<Option<TxEthSignature>>,
        fast_processing: Option<bool>,
    ) -> Result<TxHash> {
        self.tx_sender
            .submit_tx(*tx, *signature, fast_processing)
            .await
            .map_err(Error::from)
    }

    pub async fn _impl_submit_txs_batch(
        self,
        txs: Vec<TxWithSignature>,
        eth_signature: Option<TxEthSignature>,
    ) -> Result<Vec<TxHash>> {
        let txs = txs.into_iter().map(|tx| (tx.tx, tx.signature)).collect();
        self.tx_sender
            .submit_txs_batch(txs, eth_signature)
            .await
            .map_err(Error::from)
    }

    pub async fn _impl_contract_address(self) -> Result<ContractAddressResp> {
        let mut storage = self.access_storage().await?;
        let config = storage.config_schema().load_config().await.map_err(|err| {
            log::warn!(
                "[{}:{}:{}] Internal Server Error: '{}'; input: N/A",
                file!(),
                line!(),
                column!(),
                err
            );
            Error::internal_error()
        })?;

        // `expect` calls below are safe, since not having the addresses in the server config
        // means a misconfiguration, server cannot operate in this condition.
        let main_contract = config
            .contract_addr
            .expect("Server config doesn't contain the main contract address");
        let gov_contract = config
            .gov_contract_addr
            .expect("Server config doesn't contain the gov contract address");
        Ok(ContractAddressResp {
            main_contract,
            gov_contract,
        })
    }

    pub async fn _impl_tokens(self) -> Result<HashMap<String, Token>> {
        let mut storage = self.access_storage().await?;
        let mut tokens = storage.tokens_schema().load_tokens().await.map_err(|err| {
            log::warn!(
                "[{}:{}:{}] Internal Server Error: '{}'; input: N/A",
                file!(),
                line!(),
                column!(),
                err
            );
            Error::internal_error()
        })?;
        Ok(tokens
            .drain()
            .map(|(id, token)| {
                if id == 0 {
                    ("ETH".to_string(), token)
                } else {
                    (token.symbol.clone(), token)
                }
            })
            .collect())
    }

    pub async fn _impl_get_tx_fee(
        self,
        tx_type: TxFeeTypes,
        address: Address,
        token: TokenLike,
    ) -> Result<Fee> {
        Self::ticker_request(
            self.tx_sender.ticker_requests.clone(),
            tx_type,
            address,
            token,
        )
        .await
    }

    pub async fn _impl_get_txs_batch_fee_in_wei(
        self,
        tx_types: Vec<TxFeeTypes>,
        addresses: Vec<Address>,
        token: TokenLike,
    ) -> Result<BatchFee> {
        if tx_types.len() != addresses.len() {
            return Err(Error {
                code: RpcErrorCodes::IncorrectTx.into(),
                message: "Number of tx_types must be equal to the number of addresses".to_string(),
                data: None,
            });
        }

        let ticker_request_sender = self.tx_sender.ticker_requests.clone();

        let mut total_fee = BigUint::from(0u32);

        for (tx_type, address) in tx_types.iter().zip(addresses.iter()) {
            total_fee += Self::ticker_request(
                ticker_request_sender.clone(),
                tx_type.clone(),
                *address,
                token.clone(),
            )
            .await?
            .total_fee;
        }
        // Sum of transactions can be unpackable
        total_fee = closest_packable_fee_amount(&total_fee);

        Ok(BatchFee { total_fee })
    }

    pub async fn _impl_get_token_price(self, token: TokenLike) -> Result<BigDecimal> {
        Self::ticker_price_request(
            self.tx_sender.ticker_requests.clone(),
            token,
            TokenPriceRequestType::USDForOneToken,
        )
        .await
    }

    pub async fn _impl_get_eth_tx_for_withdrawal(
        self,
        withdrawal_hash: TxHash,
    ) -> Result<Option<String>> {
        self.eth_tx_for_withdrawal(withdrawal_hash).await
    }
}
