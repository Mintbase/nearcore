use std::sync::Arc;

use tracing::debug;

use near_primitives::contract::ContractCode;
use near_primitives::errors::{EpochError, StorageError};
use near_primitives::hash::CryptoHash;
use near_primitives::trie_key::{trie_key_parsers, TrieKey};
use near_primitives::types::{AccountId, Balance, EpochId, EpochInfoProvider, TrieCacheMode};
use near_primitives::utils::create_data_id;
use near_primitives::version::ProtocolVersion;
use near_store::{get_code, TrieUpdate, TrieUpdateValuePtr};
use near_vm_errors::{AnyError, VMLogicError};
use near_vm_logic::{External, ValuePtr};

pub struct RuntimeExt<'a> {
    trie_update: &'a mut TrieUpdate,
    account_id: &'a AccountId,
    action_hash: &'a CryptoHash,
    data_count: u64,
    epoch_id: &'a EpochId,
    prev_block_hash: &'a CryptoHash,
    last_block_hash: &'a CryptoHash,
    epoch_info_provider: &'a dyn EpochInfoProvider,
    current_protocol_version: ProtocolVersion,
}

/// Error used by `RuntimeExt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalError {
    /// Unexpected error which is typically related to the node storage corruption.
    /// It's possible the input state is invalid or malicious.
    StorageError(StorageError),
    /// Error when accessing validator information. Happens inside epoch manager.
    ValidatorError(EpochError),
}

impl From<ExternalError> for VMLogicError {
    fn from(err: ExternalError) -> Self {
        VMLogicError::ExternalError(AnyError::new(err))
    }
}

pub struct RuntimeExtValuePtr<'a>(TrieUpdateValuePtr<'a>);

impl<'a> ValuePtr for RuntimeExtValuePtr<'a> {
    fn len(&self) -> u32 {
        self.0.len()
    }

    fn deref(&self) -> ExtResult<Vec<u8>> {
        self.0.deref_value().map_err(wrap_storage_error)
    }
}

impl<'a> RuntimeExt<'a> {
    pub fn new(
        trie_update: &'a mut TrieUpdate,
        account_id: &'a AccountId,
        action_hash: &'a CryptoHash,
        epoch_id: &'a EpochId,
        prev_block_hash: &'a CryptoHash,
        last_block_hash: &'a CryptoHash,
        epoch_info_provider: &'a dyn EpochInfoProvider,
        current_protocol_version: ProtocolVersion,
    ) -> Self {
        RuntimeExt {
            trie_update,
            account_id,
            action_hash,
            data_count: 0,
            epoch_id,
            prev_block_hash,
            last_block_hash,
            epoch_info_provider,
            current_protocol_version,
            // #[cfg(feature = "protocol_feature_function_call_weight")]
            // gas_weights: vec![],
        }
    }

    #[inline]
    pub fn account_id(&self) -> &'a AccountId {
        self.account_id
    }

    pub fn get_code(
        &self,
        code_hash: CryptoHash,
    ) -> Result<Option<Arc<ContractCode>>, StorageError> {
        debug!(target:"runtime", "Calling the contract at account {}", self.account_id);
        let code = || get_code(self.trie_update, self.account_id, Some(code_hash));
        crate::cache::get_code(code_hash, code)
    }

    pub fn create_storage_key(&self, key: &[u8]) -> TrieKey {
        TrieKey::ContractData { account_id: self.account_id.clone(), key: key.to_vec() }
    }

    // pub fn into_receipts(self, predecessor_id: &AccountId) -> Vec<Receipt> {
    //     self.action_receipts
    //         .into_iter()
    //         .map(|(receiver_id, action_receipt)| Receipt {
    //             predecessor_id: predecessor_id.clone(),
    //             receiver_id,
    //             // Actual receipt ID is set in the Runtime.apply_action_receipt(...) in the
    //             // "Generating receipt IDs" section
    //             receipt_id: CryptoHash::default(),
    //             receipt: ReceiptEnum::Action(action_receipt),
    //         })
    //         .collect()
    // }

    // /// Appends an action and returns the index the action was inserted in the receipt
    // fn append_action(&mut self, receipt_index: u64, action: Action) -> usize {
    //     let actions = &mut self
    //         .action_receipts
    //         .get_mut(receipt_index as usize)
    //         .expect("receipt index should be present")
    //         .1
    //         .actions;

    //     actions.push(action);

    //     // Return index that action was inserted at
    //     actions.len() - 1
    // }

    pub fn set_trie_cache_mode(&mut self, state: TrieCacheMode) {
        self.trie_update.set_trie_cache_mode(state);
    }

    #[inline]
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.current_protocol_version
    }
}

fn wrap_storage_error(error: StorageError) -> VMLogicError {
    VMLogicError::from(ExternalError::StorageError(error))
}

type ExtResult<T> = ::std::result::Result<T, VMLogicError>;

impl<'a> External for RuntimeExt<'a> {
    fn storage_set(&mut self, key: &[u8], value: &[u8]) -> ExtResult<()> {
        let storage_key = self.create_storage_key(key);
        self.trie_update.set(storage_key, Vec::from(value));
        Ok(())
    }

    fn storage_get<'b>(&'b self, key: &[u8]) -> ExtResult<Option<Box<dyn ValuePtr + 'b>>> {
        let storage_key = self.create_storage_key(key);
        self.trie_update
            .get_ref(&storage_key)
            .map_err(wrap_storage_error)
            .map(|option| option.map(|ptr| Box::new(RuntimeExtValuePtr(ptr)) as Box<_>))
    }

    fn storage_remove(&mut self, key: &[u8]) -> ExtResult<()> {
        let storage_key = self.create_storage_key(key);
        self.trie_update.remove(storage_key);
        Ok(())
    }

    fn storage_has_key(&mut self, key: &[u8]) -> ExtResult<bool> {
        let storage_key = self.create_storage_key(key);
        self.trie_update.get_ref(&storage_key).map(|x| x.is_some()).map_err(wrap_storage_error)
    }

    fn storage_remove_subtree(&mut self, prefix: &[u8]) -> ExtResult<()> {
        let data_keys = self
            .trie_update
            .iter(&trie_key_parsers::get_raw_prefix_for_contract_data(self.account_id, prefix))
            .map_err(wrap_storage_error)?
            .map(|raw_key| {
                trie_key_parsers::parse_data_key_from_contract_data_key(&raw_key?, self.account_id)
                    .map_err(|_e| {
                        StorageError::StorageInconsistentState(
                            "Can't parse data key from raw key for ContractData".to_string(),
                        )
                    })
                    .map(Vec::from)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(wrap_storage_error)?;
        for key in data_keys {
            self.trie_update
                .remove(TrieKey::ContractData { account_id: self.account_id.clone(), key });
        }
        Ok(())
    }

    fn generate_data_id(&mut self) -> CryptoHash {
        let data_id = create_data_id(
            self.current_protocol_version,
            self.action_hash,
            self.prev_block_hash,
            self.last_block_hash,
            self.data_count as usize,
        );
        self.data_count += 1;
        data_id
    }

    // fn create_receipt(
    //     &mut self,
    //     receipt_indices: Vec<u64>,
    //     receiver_id: AccountId,
    // ) -> ExtResult<u64> {
    //     let mut input_data_ids = vec![];
    //     for receipt_index in receipt_indices {
    //         let data_id = self.new_data_id();
    //         self.action_receipts
    //             .get_mut(receipt_index as usize)
    //             .ok_or_else(|| HostError::InvalidReceiptIndex { receipt_index })?
    //             .1
    //             .output_data_receivers
    //             .push(DataReceiver { data_id, receiver_id: receiver_id.clone() });
    //         input_data_ids.push(data_id);
    //     }

    //     let new_receipt = ActionReceipt {
    //         signer_id: self.signer_id.clone(),
    //         signer_public_key: self.signer_public_key.clone(),
    //         gas_price: self.gas_price,
    //         output_data_receivers: vec![],
    //         input_data_ids,
    //         actions: vec![],
    //     };
    //     let new_receipt_index = self.action_receipts.len() as u64;
    //     self.action_receipts.push((receiver_id, new_receipt));
    //     Ok(new_receipt_index)
    // }

    // fn append_action_create_account(&mut self, receipt_index: u64) -> ExtResult<()> {
    //     self.append_action(receipt_index, Action::CreateAccount(CreateAccountAction {}));
    //     Ok(())
    // }

    // fn append_action_deploy_contract(
    //     &mut self,
    //     receipt_index: u64,
    //     code: Vec<u8>,
    // ) -> ExtResult<()> {
    //     self.append_action(receipt_index, Action::DeployContract(DeployContractAction { code }));
    //     Ok(())
    // }

    // #[cfg(feature = "protocol_feature_function_call_weight")]
    // fn append_action_function_call_weight(
    //     &mut self,
    //     receipt_index: u64,
    //     method_name: Vec<u8>,
    //     args: Vec<u8>,
    //     attached_deposit: u128,
    //     prepaid_gas: Gas,
    //     gas_weight: GasWeight,
    // ) -> ExtResult<()> {
    //     let action_index = self.append_action(
    //         receipt_index,
    //         Action::FunctionCall(FunctionCallAction {
    //             method_name: String::from_utf8(method_name)
    //                 .map_err(|_| HostError::InvalidMethodName)?,
    //             args,
    //             gas: prepaid_gas,
    //             deposit: attached_deposit,
    //         }),
    //     );

    //     if gas_weight.0 > 0 {
    //         self.gas_weights.push((
    //             FunctionCallActionIndex { receipt_index: receipt_index as usize, action_index },
    //             gas_weight,
    //         ));
    //     }

    //     Ok(())
    // }

    // fn append_action_function_call(
    //     &mut self,
    //     receipt_index: u64,
    //     method_name: Vec<u8>,
    //     args: Vec<u8>,
    //     attached_deposit: u128,
    //     prepaid_gas: Gas,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::FunctionCall(FunctionCallAction {
    //             method_name: String::from_utf8(method_name)
    //                 .map_err(|_| HostError::InvalidMethodName)?,
    //             args,
    //             gas: prepaid_gas,
    //             deposit: attached_deposit,
    //         }),
    //     );
    //     Ok(())
    // }

    // fn append_action_transfer(&mut self, receipt_index: u64, deposit: u128) -> ExtResult<()> {
    //     self.append_action(receipt_index, Action::Transfer(TransferAction { deposit }));
    //     Ok(())
    // }

    // fn append_action_stake(
    //     &mut self,
    //     receipt_index: u64,
    //     stake: u128,
    //     public_key: Vec<u8>,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::Stake(StakeAction {
    //             stake,
    //             public_key: PublicKey::try_from_slice(&public_key)
    //                 .map_err(|_| HostError::InvalidPublicKey)?,
    //         }),
    //     );
    //     Ok(())
    // }

    // fn append_action_add_key_with_full_access(
    //     &mut self,
    //     receipt_index: u64,
    //     public_key: Vec<u8>,
    //     nonce: u64,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::AddKey(AddKeyAction {
    //             public_key: PublicKey::try_from_slice(&public_key)
    //                 .map_err(|_| HostError::InvalidPublicKey)?,
    //             access_key: AccessKey { nonce, permission: AccessKeyPermission::FullAccess },
    //         }),
    //     );
    //     Ok(())
    // }

    // fn append_action_add_key_with_function_call(
    //     &mut self,
    //     receipt_index: u64,
    //     public_key: Vec<u8>,
    //     nonce: u64,
    //     allowance: Option<u128>,
    //     receiver_id: AccountId,
    //     method_names: Vec<Vec<u8>>,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::AddKey(AddKeyAction {
    //             public_key: PublicKey::try_from_slice(&public_key)
    //                 .map_err(|_| HostError::InvalidPublicKey)?,
    //             access_key: AccessKey {
    //                 nonce,
    //                 permission: AccessKeyPermission::FunctionCall(FunctionCallPermission {
    //                     allowance,
    //                     receiver_id: receiver_id.into(),
    //                     method_names: method_names
    //                         .into_iter()
    //                         .map(|method_name| {
    //                             String::from_utf8(method_name)
    //                                 .map_err(|_| HostError::InvalidMethodName)
    //                         })
    //                         .collect::<std::result::Result<Vec<_>, _>>()?,
    //                 }),
    //             },
    //         }),
    //     );
    //     Ok(())
    // }

    // fn append_action_delete_key(
    //     &mut self,
    //     receipt_index: u64,
    //     public_key: Vec<u8>,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::DeleteKey(DeleteKeyAction {
    //             public_key: PublicKey::try_from_slice(&public_key)
    //                 .map_err(|_| HostError::InvalidPublicKey)?,
    //         }),
    //     );
    //     Ok(())
    // }

    // fn append_action_delete_account(
    //     &mut self,
    //     receipt_index: u64,
    //     beneficiary_id: AccountId,
    // ) -> ExtResult<()> {
    //     self.append_action(
    //         receipt_index,
    //         Action::DeleteAccount(DeleteAccountAction { beneficiary_id }),
    //     );
    //     Ok(())
    // }

    fn get_touched_nodes_count(&self) -> u64 {
        self.trie_update.trie.get_touched_nodes_count()
    }

    fn validator_stake(&self, account_id: &AccountId) -> ExtResult<Option<Balance>> {
        self.epoch_info_provider
            .validator_stake(self.epoch_id, self.prev_block_hash, account_id)
            .map_err(|e| ExternalError::ValidatorError(e).into())
    }

    fn validator_total_stake(&self) -> ExtResult<Balance> {
        self.epoch_info_provider
            .validator_total_stake(self.epoch_id, self.prev_block_hash)
            .map_err(|e| ExternalError::ValidatorError(e).into())
    }

    // /// Distributes the gas passed in by splitting it among weights defined in `gas_weights`.
    // /// This will sum all weights, retrieve the gas per weight, then update each function
    // /// to add the respective amount of gas. Once all gas is distributed, the remainder of
    // /// the gas not assigned due to precision loss is added to the last function with a weight.
    // #[cfg(feature = "protocol_feature_function_call_weight")]
    // fn distribute_unused_gas(&mut self, gas: u64) -> GasDistribution {
    //     let gas_weight_sum: u128 =
    //         self.gas_weights.iter().map(|(_, GasWeight(weight))| *weight as u128).sum();
    //     if gas_weight_sum != 0 {
    //         // Floor division that will ensure gas allocated is <= gas to distribute
    //         let gas_per_weight = (gas as u128 / gas_weight_sum) as u64;

    //         let mut distribute_gas = |metadata: &FunctionCallActionIndex, assigned_gas: u64| {
    //             let FunctionCallActionIndex { receipt_index, action_index } = metadata;
    //             if let Some(Action::FunctionCall(FunctionCallAction { ref mut gas, .. })) = self
    //                 .action_receipts
    //                 .get_mut(*receipt_index)
    //                 .and_then(|(_, receipt)| receipt.actions.get_mut(*action_index))
    //             {
    //                 *gas += assigned_gas;
    //             } else {
    //                 panic!(
    //                     "Invalid index for assigning unused gas weight \
    //                     (promise_index={}, action_index={})",
    //                     receipt_index, action_index
    //                 );
    //             }
    //         };

    //         let mut distributed = 0;
    //         for (action_index, GasWeight(weight)) in &self.gas_weights {
    //             // This can't overflow because the gas_per_weight is floor division
    //             // of the weight sum.
    //             let assigned_gas = gas_per_weight * weight;

    //             distribute_gas(action_index, assigned_gas);

    //             distributed += assigned_gas
    //         }

    //         // Distribute remaining gas to final action.
    //         if let Some((last_idx, _)) = self.gas_weights.last() {
    //             distribute_gas(last_idx, gas - distributed);
    //         }
    //         self.gas_weights.clear();
    //         GasDistribution::All
    //     } else {
    //         GasDistribution::NoRatios
    //     }
    // }
}
