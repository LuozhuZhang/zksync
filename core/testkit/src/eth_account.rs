use bigdecimal::BigDecimal;
use eth_client::ETHClient;
use ethabi::ParamType;
use failure::{ensure, format_err};
use futures::compat::Future01CompatExt;
use models::abi::{erc20_contract, zksync_contract};
use models::config_options::ConfigurationOptions;
use models::node::block::Block;
use models::node::{AccountId, Address, Nonce, PriorityOp, PubKeyHash, TokenId};
use models::primitives::big_decimal_to_u128;
use models::EncodedProof;
use std::convert::TryFrom;
use std::str::FromStr;
use std::time::Duration;
use web3::contract::{Contract, Options};
use web3::types::{
    BlockNumber, CallRequest, Transaction, TransactionId, TransactionReceipt, H256, U128, U256, U64,
};
use web3::{Transport, Web3};

const WEB3_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub fn parse_ether(eth_value: &str) -> Result<BigDecimal, failure::Error> {
    let split = eth_value.split('.').collect::<Vec<&str>>();
    ensure!(split.len() == 1 || split.len() == 2, "Wrong eth value");
    let string_wei_value = if split.len() == 1 {
        format!("{}000000000000000000", split[0])
    } else if split.len() == 2 {
        let before_dot = split[0];
        let after_dot = split[1];
        ensure!(
            after_dot.len() <= 18,
            "ETH value can have up to 18 digits after dot."
        );
        let zeros_to_pad = 18 - after_dot.len();
        format!("{}{}{}", before_dot, after_dot, "0".repeat(zeros_to_pad))
    } else {
        unreachable!()
    };

    Ok(BigDecimal::from_str(&string_wei_value)?)
}

/// Used to sign and post ETH transactions for the ZK Sync contracts.
#[derive(Debug)]
pub struct EthereumAccount<T: Transport> {
    pub private_key: H256,
    pub address: Address,
    pub main_contract_eth_client: ETHClient<T>,
}

fn big_dec_to_u256(bd: BigDecimal) -> U256 {
    U256::from_dec_str(&bd.to_string()).unwrap()
}

fn u256_to_big_dec(u256: U256) -> BigDecimal {
    BigDecimal::from_str(&u256.to_string()).unwrap()
}

impl<T: Transport> EthereumAccount<T> {
    pub fn new(
        private_key: H256,
        address: Address,
        transport: T,
        contract_address: Address,
        config: &ConfigurationOptions,
    ) -> Self {
        let main_contract_eth_client = ETHClient::new(
            transport,
            zksync_contract(),
            address,
            private_key,
            contract_address,
            config.chain_id,
            config.gas_price_factor,
        );

        Self {
            private_key,
            address,
            main_contract_eth_client,
        }
    }

    pub async fn total_blocks_committed(&self) -> Result<u64, failure::Error> {
        let contract = Contract::new(
            self.main_contract_eth_client.web3.eth(),
            self.main_contract_eth_client.contract_addr,
            self.main_contract_eth_client.contract.clone(),
        );

        contract
            .query("totalBlocksCommitted", (), None, Options::default(), None)
            .compat()
            .await
            .map_err(|e| format_err!("Contract query fail: {}", e))
    }

    pub async fn is_exodus(&self) -> Result<bool, failure::Error> {
        let contract = Contract::new(
            self.main_contract_eth_client.web3.eth(),
            self.main_contract_eth_client.contract_addr,
            self.main_contract_eth_client.contract.clone(),
        );

        contract
            .query("exodusMode", (), None, Options::default(), None)
            .compat()
            .await
            .map_err(|e| format_err!("Contract query fail: {}", e))
    }

    pub async fn full_exit(
        &self,
        account_id: AccountId,
        token_address: Address,
    ) -> Result<PriorityOp, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "fullExit",
                (u64::from(account_id), token_address),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Full exit send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Full exit wait confirm err: {}", e))?;
        ensure!(
            receipt.status == Some(U64::from(1)),
            "Full exit submit fail"
        );
        Ok(receipt
            .logs
            .into_iter()
            .map(PriorityOp::try_from)
            .filter_map(|op| op.ok())
            .next()
            .expect("no priority op log in full exit"))
    }

    pub async fn exit(
        &self,
        token_id: TokenId,
        amount: &BigDecimal,
        proof: EncodedProof,
    ) -> Result<ETHExecResult, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "exit",
                (
                    u64::from(token_id),
                    U128::from(big_decimal_to_u128(amount)),
                    proof,
                ),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Exit send err: {}", e))?;

        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Exit wait confirm err: {}", e))?;

        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    pub async fn cancel_outstanding_deposits_for_exodus_mode(
        &self,
        number: u64,
    ) -> Result<ETHExecResult, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "cancelOutstandingDepositsForExodusMode",
                number,
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("cancelOutstandingDepositsForExodusMode send err: {}", e))?;

        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| {
                format_err!(
                    "cancelOutstandingDepositsForExodusMode wait confirm err: {}",
                    e
                )
            })?;

        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    pub async fn change_pubkey_priority_op(
        &self,
        new_pubkey_hash: &PubKeyHash,
    ) -> Result<PriorityOp, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "changePubKeyHash",
                (new_pubkey_hash.data.to_vec(),),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("ChangePubKeyHash send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("ChangePubKeyHash wait confirm err: {}", e))?;
        ensure!(
            receipt.status == Some(U64::from(1)),
            "ChangePubKeyHash transaction failed"
        );
        Ok(receipt
            .logs
            .into_iter()
            .map(PriorityOp::try_from)
            .filter_map(|op| op.ok())
            .next()
            .expect("no priority op log in change pubkey hash"))
    }

    pub async fn deposit_eth(
        &self,
        amount: BigDecimal,
        to: &Address,
        nonce: Option<U256>,
    ) -> Result<PriorityOp, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "depositETH",
                *to,
                Options::with(|opt| {
                    opt.value = Some(big_dec_to_u256(amount.clone()));
                    opt.nonce = nonce;
                }),
            )
            .await
            .map_err(|e| format_err!("Deposit eth send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Deposit eth wait confirm err: {}", e))?;
        ensure!(receipt.status == Some(U64::from(1)), "eth deposit fail");
        Ok(receipt
            .logs
            .into_iter()
            .map(PriorityOp::try_from)
            .filter_map(|op| op.ok())
            .next()
            .expect("no priority op log in deposit"))
    }

    pub async fn eth_balance(&self) -> Result<BigDecimal, failure::Error> {
        Ok(u256_to_big_dec(
            self.main_contract_eth_client
                .web3
                .eth()
                .balance(self.address.clone(), None)
                .compat()
                .await?,
        ))
    }

    pub async fn erc20_balance(
        &self,
        token_contract: &Address,
    ) -> Result<BigDecimal, failure::Error> {
        let contract = Contract::new(
            self.main_contract_eth_client.web3.eth(),
            *token_contract,
            erc20_contract(),
        );
        contract
            .query("balanceOf", self.address, None, Options::default(), None)
            .compat()
            .await
            .map(u256_to_big_dec)
            .map_err(|e| format_err!("Contract query fail: {}", e))
    }

    pub async fn balances_to_withdraw(&self, token: TokenId) -> Result<BigDecimal, failure::Error> {
        let contract = Contract::new(
            self.main_contract_eth_client.web3.eth(),
            self.main_contract_eth_client.contract_addr,
            self.main_contract_eth_client.contract.clone(),
        );

        Ok(contract
            .query(
                "balancesToWithdraw",
                (self.address, u64::from(token)),
                None,
                Options::default(),
                None,
            )
            .compat()
            .await
            .map(u256_to_big_dec)
            .map_err(|e| format_err!("Contract query fail: {}", e))?)
    }

    pub async fn approve_erc20(
        &self,
        token_contract: Address,
        amount: BigDecimal,
    ) -> Result<(), failure::Error> {
        let erc20_client = ETHClient::new(
            self.main_contract_eth_client.web3.transport().clone(),
            erc20_contract(),
            self.address,
            self.private_key,
            token_contract,
            self.main_contract_eth_client.chain_id,
            self.main_contract_eth_client.gas_price_factor,
        );

        let signed_tx = erc20_client
            .sign_call_tx(
                "approve",
                (
                    self.main_contract_eth_client.contract_addr,
                    big_dec_to_u256(amount.clone()),
                ),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Approve send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Approve wait confirm err: {}", e))?;

        ensure!(receipt.status == Some(U64::from(1)), "erc20 approve fail");

        Ok(())
    }

    pub async fn deposit_erc20(
        &self,
        token_contract: Address,
        amount: BigDecimal,
        to: &Address,
    ) -> Result<PriorityOp, failure::Error> {
        self.approve_erc20(token_contract, amount.clone()).await?;

        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "depositERC20",
                (token_contract, big_dec_to_u256(amount.clone()), *to),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Deposit erc20 send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Deposit erc20 wait confirm err: {}", e))?;
        ensure!(receipt.status == Some(U64::from(1)), "erc20 deposit fail");
        Ok(receipt
            .logs
            .into_iter()
            .map(PriorityOp::try_from)
            .filter_map(|op| op.ok())
            .next()
            .expect("no priority op log in deposit"))
    }

    pub async fn commit_block(&self, block: &Block) -> Result<ETHExecResult, failure::Error> {
        let witness_data = block.get_eth_witness_data();
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "commitBlock",
                (
                    u64::from(block.block_number),
                    u64::from(block.fee_account),
                    block.get_eth_encoded_root(),
                    block.get_eth_public_data(),
                    witness_data.0,
                    witness_data.1,
                ),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Commit block send err: {}", e))?;

        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Commit block confirm err: {}", e))?;

        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    // Verifies block using empty proof. (`DUMMY_VERIFIER` should be enabled on the contract).
    pub async fn verify_block(&self, block: &Block) -> Result<ETHExecResult, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "verifyBlock",
                (
                    u64::from(block.block_number),
                    [U256::default(); 8],
                    block.get_withdrawals_data(),
                ),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Verify block send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Verify block confirm err: {}", e))?;
        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    // Completes pending withdrawals.
    pub async fn complete_withdrawals(&self) -> Result<ETHExecResult, failure::Error> {
        let max_withdrawals_to_complete: u64 = 999;
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "completeWithdrawals",
                max_withdrawals_to_complete,
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("Complete withdrawals send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Complete withdrawals confirm err: {}", e))?;

        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    pub async fn trigger_exodus_if_needed(&self) -> Result<ETHExecResult, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx("triggerExodusIfNeeded", (), Options::default())
            .await
            .map_err(|e| format_err!("Trigger exodus if needed send err: {}", e))?;
        let receipt = self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(signed_tx.raw_tx.into(), WEB3_POLL_INTERVAL, 1)
            .compat()
            .await
            .map_err(|e| format_err!("Trigger exodus if needed confirm err: {}", e))?;

        Ok(ETHExecResult::new(receipt, &self.main_contract_eth_client.web3).await)
    }

    pub async fn eth_block_number(&self) -> Result<u64, failure::Error> {
        Ok(self.main_contract_eth_client.block_number().await?.as_u64())
    }

    pub async fn auth_fact(
        &self,
        fact: &[u8],
        nonce: Nonce,
    ) -> Result<TransactionReceipt, failure::Error> {
        let signed_tx = self
            .main_contract_eth_client
            .sign_call_tx(
                "authPubkeyHash",
                (fact.to_vec(), u64::from(nonce)),
                Options::default(),
            )
            .await
            .map_err(|e| format_err!("AuthFact send err: {}", e))?;
        Ok(self
            .main_contract_eth_client
            .web3
            .send_raw_transaction_with_confirmation(
                signed_tx.raw_tx.into(),
                Duration::from_millis(500),
                1,
            )
            .compat()
            .await
            .map_err(|e| format_err!("AuthFact confirm err: {}", e))?)
    }
}

#[derive(Debug, Clone)]
pub struct ETHExecResult {
    success: bool,
    receipt: TransactionReceipt,
    revert_reason: String,
}

impl ETHExecResult {
    pub async fn new<T: Transport>(receipt: TransactionReceipt, web3: &Web3<T>) -> Self {
        let (success, revert_reason) = if receipt.status == Some(U64::from(1)) {
            (true, String::from(""))
        } else {
            let reason = get_revert_reason(&receipt, web3)
                .await
                .expect("Failed to get revert reason");
            (false, reason)
        };

        Self {
            success,
            revert_reason,
            receipt,
        }
    }

    pub fn expect_success(self) {
        if !self.success {
            panic!(
                "Expected transaction success, revert reason: {}, tx: 0x{:x}",
                self.revert_reason, self.receipt.transaction_hash
            );
        }
    }

    pub fn expect_revert(self, code: &str) {
        if self.success {
            panic!(
                "Expected transaction fail, success instead, tx: 0x{:x}",
                self.receipt.transaction_hash
            );
        } else if self.revert_reason != code {
            panic!("Transaction failed with incorrect return code, expected: {}, found: {}, tx: 0x{:x}", code, self.revert_reason, self.receipt.transaction_hash);
        }
    }
}

/// Gets revert reason of failed transactions (i.e. if contract executes `require(false, "msg")` this function returns "msg")
async fn get_revert_reason<T: Transport>(
    receipt: &TransactionReceipt,
    web3: &Web3<T>,
) -> Result<String, failure::Error> {
    let tx = web3
        .eth()
        .transaction(TransactionId::Hash(receipt.transaction_hash))
        .compat()
        .await?;
    if let Some(Transaction {
        from,
        to: Some(to),
        gas,
        gas_price,
        value,
        input,
        ..
    }) = tx
    {
        // To get revert reason we have to make call to contract using the same args as function.
        let encoded_revert_reason = web3
            .eth()
            .call(
                CallRequest {
                    from: Some(from),
                    to,
                    gas: Some(gas),
                    gas_price: Some(gas_price),
                    value: Some(value),
                    data: Some(input),
                },
                receipt
                    .block_number
                    .clone()
                    .map(|n| BlockNumber::Number(n.as_u64())),
            )
            .compat()
            .await?;
        // This function returns ABI encoded retrun value for function with signature "Error(string)"
        // we strip first 4 bytes because they encode function name "Error", the rest is encoded string.
        let encoded_string_without_function_hash = &encoded_revert_reason.0[4..];
        Ok(
            ethabi::decode(&[ParamType::String], encoded_string_without_function_hash)
                .map_err(|e| format_err!("ABI decode error {}", e))?
                .into_iter()
                .next()
                .unwrap()
                .to_string()
                .unwrap(),
        )
    } else {
        Ok("".to_string())
    }
}
