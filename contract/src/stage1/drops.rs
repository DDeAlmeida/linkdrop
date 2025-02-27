use crate::*;
use near_sdk::{
    collections::{LazyOption, Vector},
    require, Balance,
};

pub type DropId = u128;

#[derive(BorshSerialize, BorshDeserialize)]
pub enum DropType {
    Simple,
    NonFungibleToken(NFTData),
    FungibleToken(FTData),
    FunctionCall(FCData),
}

#[derive(BorshSerialize, BorshDeserialize, Deserialize, Serialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub enum ClaimPermissions {
    Claim,
    CreateAccountAndClaim,
}

/// Keep track of different configuration options for each key in a drop
#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct KeyInfo {
    // How many uses this key has left. Once 0 is reached, the key is deleted
    pub remaining_uses: u64,

    // When was the last time the key was used
    pub last_used: u64,

    // How much allowance does the key have left. When the key is deleted, this is refunded to the funder's balance.
    pub allowance: u128,

    // Nonce for the current key.
    pub key_id: u64,
}

/// Keep track of different configuration options for each key in a drop
#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct DropConfig {
    // How many claims can each key have. If None, default to 1.
    pub uses_per_key: Option<u64>,

    // Minimum block timestamp that keys can be used. If None, keys can be used immediately
    // Measured in number of non-leap-nanoseconds since January 1, 1970 0:00:00 UTC.
    pub start_timestamp: Option<u64>,

    // How often can a key be used
    // Measured in number of non-leap-nanoseconds since January 1, 1970 0:00:00 UTC.
    pub throttle_timestamp: Option<u64>,

    // If claim is called, refund the deposit to the owner's balance. If None, default to false.
    pub on_claim_refund_deposit: Option<bool>,

    // Can the access key only call the claim method_name? Default to both method_name callable
    pub claim_permission: Option<ClaimPermissions>,

    // Root account that all sub-accounts will default to. If None, default to the global drop root.
    pub drop_root: Option<AccountId>,
}

// Drop Metadata should be a string which can be JSON or anything the users want.
pub type DropMetadata = String;

/// Keep track of specific data related to an access key. This allows us to optionally refund funders later.
#[derive(BorshDeserialize, BorshSerialize)]
pub struct Drop {
    // Funder of this specific drop
    pub owner_id: AccountId,
    // Set of public keys associated with this drop mapped to their usages
    pub pks: UnorderedMap<PublicKey, KeyInfo>,

    // Balance for all keys of this drop. Can be 0 if specified.
    pub deposit_per_use: u128,

    // How many uses are registered (for FTs and NFTs)
    pub registered_uses: u64,

    // Ensure this drop can only be used when the function has the required gas to attach
    pub required_gas: Gas,

    // Every drop must have a type
    pub drop_type: DropType,

    // The drop as a whole can have a config as well
    pub config: Option<DropConfig>,

    // Metadata for the drop
    pub metadata: LazyOption<DropMetadata>,

    // Keep track of the next nonce to give out to a key
    pub next_key_id: u64,
}

#[near_bindgen]
impl Keypom {
    /*
        user has created a bunch of keypairs and passed in the public keys and attached some attached_deposit.
        this will store the account data and allow that keys to call claim and create_account_and_claim
        on this contract.

        The balance is the amount of $NEAR the sender wants each linkdrop to contain.
    */
    #[payable]
    pub fn create_drop(
        &mut self,
        public_keys: Vec<PublicKey>,
        deposit_per_use: U128,
        config: Option<DropConfig>,
        metadata: Option<DropMetadata>,
        ft_data: Option<FTDataConfig>,
        nft_data: Option<NFTDataConfig>,
        fc_data: Option<FCData>,
    ) -> DropId {
        // Ensure the user has only specified one type of callback data
        let num_cbs_specified =
            ft_data.is_some() as u8 + nft_data.is_some() as u8 + fc_data.is_some() as u8;
        require!(
            num_cbs_specified <= 1,
            "You cannot specify more than one callback data"
        );

        // Warn if the balance for each drop is less than the minimum
        if deposit_per_use.0 < NEW_ACCOUNT_BASE {
            near_sdk::log!(
                "Warning: Balance is less than absolute minimum for creating an account: {}",
                NEW_ACCOUNT_BASE
            );
        }

        // Funder is the predecessor
        let owner_id = env::predecessor_account_id();
        let len = public_keys.len() as u128;
        let drop_id = self.next_drop_id;
        // Get the number of claims per key to dictate what key usage data we should put in the map
        let num_claims_per_key = config.clone().and_then(|c| c.uses_per_key).unwrap_or(1);

        // Get the current balance of the funder.
        let mut current_user_balance = self
            .user_balances
            .get(&owner_id)
            .expect("No user balance found");
        near_sdk::log!("Cur User balance {}", yocto_to_near(current_user_balance));

        // Pessimistically measure storage
        let initial_storage = env::storage_usage();
        let mut key_map: UnorderedMap<PublicKey, KeyInfo> =
            UnorderedMap::new(StorageKey::PksForDrop {
                // We get a new unique prefix for the collection
                account_id_hash: hash_account_id(&format!("{}{}", self.next_drop_id, owner_id)),
            });

        // Decide what methods the access keys can call
        let mut access_key_method_names = ACCESS_KEY_BOTH_METHOD_NAMES;
        if let Some(perms) = config.clone().and_then(|c| c.claim_permission) {
            match perms {
                // If we have a config, use the config to determine what methods the access keys can call
                ClaimPermissions::Claim => {
                    access_key_method_names = ACCESS_KEY_CLAIM_METHOD_NAME;
                }
                ClaimPermissions::CreateAccountAndClaim => {
                    access_key_method_names = ACCESS_KEY_CREATE_ACCOUNT_METHOD_NAME;
                }
            }
        }

        // Default the gas to attach to be the gas from the wallet. This will be used to calculate allowances.
        let mut gas_to_attach = ATTACHED_GAS_FROM_WALLET;
        // Depending on the FC Data, set the Gas to attach and the access key method_name names
        if let Some(gas) = fc_data
            .clone()
            .and_then(|d| d.config.and_then(|c| c.attached_gas))
        {
            require!(
                deposit_per_use.0 == 0,
                "cannot specify gas to attach and have a balance in the linkdrop"
            );
            require!(
                gas <= ATTACHED_GAS_FROM_WALLET - GAS_OFFSET_IF_FC_EXECUTE,
                &format!(
                    "cannot attach more than {:?} GAS.",
                    ATTACHED_GAS_FROM_WALLET - GAS_OFFSET_IF_FC_EXECUTE
                )
            );
            gas_to_attach = gas + GAS_OFFSET_IF_FC_EXECUTE;
            access_key_method_names = ACCESS_KEY_CLAIM_METHOD_NAME;
        }

        // Calculate the base allowance to attach
        let calculated_base_allowance = self.calculate_base_allowance(gas_to_attach);
        // The actual allowance is the base * number of claims per key since each claim can potentially use the max pessimistic GAS.
        let actual_allowance = calculated_base_allowance * num_claims_per_key as u128;

        // Loop through and add each drop ID to the public keys. Also populate the key set.
        let mut next_key_id = 0;
        for pk in &public_keys {
            key_map.insert(
                pk,
                &KeyInfo {
                    remaining_uses: num_claims_per_key,
                    last_used: 0, // Set to 0 since this will make the key always claimable.
                    allowance: actual_allowance,
                    key_id: next_key_id,
                },
            );
            require!(
                self.drop_id_for_pk.insert(pk, &drop_id).is_none(),
                "Keys cannot belong to another drop"
            );
            next_key_id += 1;
        }

        // Add this drop ID to the funder's set of drops
        self.internal_add_drop_to_funder(&env::predecessor_account_id(), &drop_id);

        // Create drop object
        let mut drop = Drop {
            owner_id: env::predecessor_account_id(),
            deposit_per_use: deposit_per_use.0,
            pks: key_map,
            drop_type: DropType::Simple, // Default to simple but will overwrite if not
            config: config.clone(),
            registered_uses: num_claims_per_key * len as u64,
            required_gas: gas_to_attach,
            metadata: LazyOption::new(
                StorageKey::DropMetadata {
                    // We get a new unique prefix for the collection
                    account_id_hash: hash_account_id(&format!(
                        "metadata-{}{}",
                        self.next_drop_id, owner_id
                    )),
                },
                metadata.as_ref(),
            ),
            next_key_id,
        };

        // For NFT drops, measure the storage for adding the longest token ID
        let mut storage_per_longest = 0;
        // Keep track of the total attached_deposit required for the FC data (depending on None and Some cases)
        let mut deposit_required_for_fc_deposits = 0;
        // Keep track of the number of none FCs so we don't charge the user
        let mut num_none_fcs = 0;
        // If NFT data was provided, we need to build the set of token IDs and cast the config to actual NFT data
        if let Some(data) = nft_data {
            let NFTDataConfig {
                sender_id,
                contract_id,
                longest_token_id,
            } = data;

            // Create the token ID vector and insert the longest token ID
            let token_ids = Vector::new(StorageKey::TokenIdsForDrop {
                //we get a new unique prefix for the collection
                account_id_hash: hash_account_id(&format!("nft-{}{}", self.next_drop_id, owner_id)),
            });

            // Create the NFT data
            let actual_nft_data = NFTData {
                sender_id,
                contract_id,
                longest_token_id: longest_token_id.clone(),
                storage_for_longest: u128::MAX,
                token_ids,
            };

            // The number of claims is 0 until NFTs are sent to the contract
            drop.registered_uses = 0;
            drop.drop_type = DropType::NonFungibleToken(actual_nft_data);

            // Add the drop with the empty token IDs
            self.drop_for_id.insert(&drop_id, &drop);

            // Measure how much storage it costs to insert the 1 longest token ID
            let initial_nft_storage_one = env::storage_usage();
            // Now that the drop has been added, insert the longest token ID and measure storage
            if let DropType::NonFungibleToken(data) = &mut drop.drop_type {
                data.token_ids.push(&longest_token_id);
            }

            // Add drop with the longest possible token ID and max storage
            self.drop_for_id.insert(&drop_id, &drop);
            let final_nft_storage_one = env::storage_usage();
            near_sdk::log!(
                "i1: {} f1: {}",
                initial_nft_storage_one,
                final_nft_storage_one
            );

            // Measure the storage per single longest token ID
            storage_per_longest = Balance::from(final_nft_storage_one - initial_nft_storage_one);
            near_sdk::log!(
                "TOKENS BEFORE {:?}",
                self.get_nft_token_ids_for_drop(self.next_drop_id, None, None)
            );

            // Clear the token IDs so it's an empty vector and put the storage in the drop's nft data
            if let DropType::NonFungibleToken(data) = &mut drop.drop_type {
                data.token_ids.pop();
                data.storage_for_longest = storage_per_longest;
            }

            self.drop_for_id.insert(&drop_id, &drop);
        } else if let Some(data) = ft_data.clone() {
            // If FT Data was provided, we need to cast the FT Config to actual FT data and insert into the drop type
            let FTDataConfig {
                sender_id,
                contract_id,
                balance_per_use,
            } = data;

            // Create the NFT data
            let actual_ft_data = FTData {
                contract_id,
                sender_id,
                balance_per_use,
                ft_storage: U128(u128::MAX),
            };

            // The number of claims is 0 until FTs are sent to the contract
            drop.registered_uses = 0;
            drop.drop_type = DropType::FungibleToken(actual_ft_data);

            // Add the drop with the empty token IDs
            self.drop_for_id.insert(&drop_id, &drop);
        } else if let Some(data) = fc_data.clone() {
            drop.drop_type = DropType::FunctionCall(data.clone());

            // Ensure proper method data is passed in
            let num_method_data = data.clone().methods.len() as u64;
            // If there's 1 claim, there should be 1 method data defined
            if num_claims_per_key == 1 {
                require!(
                    num_method_data == 1,
                    "Cannot have more Method Data than the number of claims per key"
                );
            // If there's more than 1 method data defined and the number of claims per key more than 1, the number of methods should equal the number of claims per key
            } else if num_method_data > 1 {
                require!(
                    num_method_data == num_claims_per_key,
                    "Number of FCs must match number of claims per key if more than 1 is specified"
                );
            }

            // If there's one method data specified and more than 1 claim per key, that data is to be used
            // For all the claims. In this case, we need to tally all the deposits for each method in all method data.
            if num_claims_per_key > 1 && num_method_data == 1 {
                let attached_deposit = data
                    .methods
                    .iter()
                    .next()
                    .unwrap()
                    .clone()
                    .expect("cannot have a single none function call")
                    // iterate through   all entries and sum the attached_deposit
                    .iter()
                    .fold(0, |acc, x| acc + x.attached_deposit.0);

                near_sdk::log!(format!(
                    "Total attached_deposits for all method data: {}",
                    attached_deposit
                )
                .as_str());
                deposit_required_for_fc_deposits = num_claims_per_key as u128 * attached_deposit;
            // In the case where either there's 1 claim per key or the number of FCs is not 1,
            // We can simply loop through and manually get this data
            } else {
                for method_name in data.methods {
                    num_none_fcs += method_name.is_none() as u64;
                    // If the method is not None, we need to get the attached_deposit by looping through the method datas
                    if let Some(method_data) = method_name {
                        let attached_deposit = method_data
                            .iter()
                            .fold(0, |acc, x| acc + x.attached_deposit.0);
                        near_sdk::log!(
                            format!("Adding attached deposit: {}", attached_deposit).as_str()
                        );
                        deposit_required_for_fc_deposits += attached_deposit;
                    }
                }
            }

            // Add the drop with the empty token IDs
            self.drop_for_id.insert(&drop_id, &drop);
        } else {
            require!(
                deposit_per_use.0 > 0,
                "Cannot have a simple drop with zero balance"
            );
            // In simple case, we just insert the drop with whatever it was initialized with.
            self.drop_for_id.insert(&drop_id, &drop);
        }

        // Calculate the storage being used for the entire drop
        let final_storage = env::storage_usage();
        let total_required_storage = Balance::from(final_storage - initial_storage)
            * env::storage_byte_cost();
        near_sdk::log!("Total required storage Yocto {}", total_required_storage);

        // Increment the drop ID nonce
        self.next_drop_id += 1;

        /*
            Required attached_deposit consists of:
            - Fees
            - TOTAL Storage
            - Total access key allowance for EACH key
            - Access key storage for EACH key
            - Balance for each key * (number of claims - claims with None for FC Data)

            Optional:
            - FC attached_deposit for each key * num Some(data) claims
            - storage for longest token ID for each key
            - FT storage registration cost for each key * claims (calculated in resolve storage calculation function)
        */
        let fees = self
            .fees_per_user
            .get(&owner_id)
            .unwrap_or((self.drop_fee, self.key_fee));
        let required_deposit = fees.0 // drop fee
            + total_required_storage
            + (fees.1 // key fee
                + actual_allowance
                + ACCESS_KEY_STORAGE
                + deposit_per_use.0 * (num_claims_per_key - num_none_fcs) as u128
                + storage_per_longest * env::storage_byte_cost() * (num_claims_per_key - num_none_fcs) as u128
                + deposit_required_for_fc_deposits)
                * len;
        near_sdk::log!(
            "Current balance: {}, 
            Required Deposit: {}, 
            Drop Fee: {}, 
            Total Required Storage: {}, 
            Key Fee: {}, 
            ACCESS_KEY_ALLOWANCE: {}, 
            ACCESS_KEY_STORAGE: {},
            Linkdrop Balance: {}, 
            Storage for longest token ID (if applicable): {},
            total function call deposits (if applicable): {},
            Num claims per key: {}
            Num none FCs: {},
            length: {}
            GAS to attach: {}",
            yocto_to_near(current_user_balance),
            yocto_to_near(required_deposit),
            yocto_to_near(fees.0),
            yocto_to_near(total_required_storage),
            yocto_to_near(fees.1),
            yocto_to_near(actual_allowance),
            yocto_to_near(ACCESS_KEY_STORAGE),
            yocto_to_near(deposit_per_use.0),
            yocto_to_near(storage_per_longest * env::storage_byte_cost()),
            yocto_to_near(deposit_required_for_fc_deposits),
            num_claims_per_key,
            num_none_fcs,
            len,
            gas_to_attach.0
        );

        /*
            Ensure the attached attached_deposit can cover:
        */
        require!(
            current_user_balance >= required_deposit,
            "Not enough attached_deposit"
        );
        // Decrement the user's balance by the required attached_deposit and insert back into the map
        current_user_balance -= required_deposit;
        self.user_balances.insert(&owner_id, &current_user_balance);
        near_sdk::log!("New user balance {}", yocto_to_near(current_user_balance));

        // Increment our fees earned
        self.fees_collected += fees.0 + fees.1 * len;
        near_sdk::log!("Fees collected {}", yocto_to_near(fees.0 + fees.1 * len));

        let current_account_id = env::current_account_id();

        /*
            Only add the access keys if it's not a FT drop. If it is,
            keys will be added in the FT resolver
        */
        if ft_data.is_none() {
            // Create a new promise batch to create all the access keys
            let promise = env::promise_batch_create(&current_account_id);

            // Loop through each public key and create the access keys
            for pk in public_keys.clone() {
                // Must assert in the loop so no access keys are made?
                env::promise_batch_action_add_key_with_function_call(
                    promise,
                    &pk,
                    0,
                    actual_allowance,
                    &current_account_id,
                    access_key_method_names,
                );
            }

            env::promise_return(promise);
        } else {
            /*
                Get the storage required by the FT contract and ensure the user has attached enough
                attached_deposit to cover the storage and perform refunds if they overpayed.
            */

            ext_ft_contract::ext(ft_data.unwrap().contract_id)
                // Call storage balance bounds with exactly this amount of GAS. No unspent GAS will be added on top.
                .with_static_gas(GAS_FOR_STORAGE_BALANCE_BOUNDS)
                .with_unused_gas_weight(0)
                .storage_balance_bounds()
                .then(
                    Self::ext(current_account_id)
                        // Resolve the promise with the min GAS. All unspent GAS will be added to this call.
                        .with_static_gas(MIN_GAS_FOR_RESOLVE_STORAGE_CHECK)
                        .resolve_storage_check(public_keys, drop_id, required_deposit),
                );
        }

        drop_id
    }

    /*
        Allows users to add to an existing drop.
        Only the funder can call this method_name
    */
    #[payable]
    pub fn add_keys(&mut self, public_keys: Vec<PublicKey>, drop_id: DropId) -> DropId {
        let mut drop = self
            .drop_for_id
            .get(&drop_id)
            .expect("no drop found for ID");
        let config = &drop.config;
        let funder = &drop.owner_id;

        require!(
            funder == &env::predecessor_account_id(),
            "only funder can add to drops"
        );

        let len = public_keys.len() as u128;

        /*
            Add data to storage
        */
        // Pessimistically measure storage
        let initial_storage = env::storage_usage();

        // Get the number of claims per key
        let num_claims_per_key = config.clone().and_then(|c| c.uses_per_key).unwrap_or(1);

        // get the existing key set and add new PKs
        let mut exiting_key_map = drop.pks;

        // Calculate the base allowance to attach
        let calculated_base_allowance = self.calculate_base_allowance(drop.required_gas);
        // The actual allowance is the base * number of claims per key since each claim can potentially use the max pessimistic GAS.
        let actual_allowance = calculated_base_allowance * num_claims_per_key as u128;
        // Loop through and add each drop ID to the public keys. Also populate the key set.
        let mut next_key_id = drop.next_key_id;
        for pk in public_keys.clone() {
            exiting_key_map.insert(
                &pk,
                &KeyInfo {
                    remaining_uses: num_claims_per_key,
                    last_used: 0, // Set to 0 since this will make the key always claimable.
                    allowance: actual_allowance,
                    key_id: next_key_id,
                },
            );
            require!(
                self.drop_id_for_pk.insert(&pk, &drop_id).is_none(),
                "Keys cannot belong to another drop"
            );
            next_key_id += 1;
        }

        // Set the drop's PKs to the newly populated set
        drop.pks = exiting_key_map;
        // Set the drop's current key nonce
        drop.next_key_id = next_key_id;

        // Decide what methods the access keys can call
        // Decide what methods the access keys can call
        let mut access_key_method_names = ACCESS_KEY_BOTH_METHOD_NAMES;
        if let Some(perms) = config.clone().and_then(|c| c.claim_permission) {
            match perms {
                // If we have a config, use the config to determine what methods the access keys can call
                ClaimPermissions::Claim => {
                    access_key_method_names = ACCESS_KEY_CLAIM_METHOD_NAME;
                }
                ClaimPermissions::CreateAccountAndClaim => {
                    access_key_method_names = ACCESS_KEY_CREATE_ACCOUNT_METHOD_NAME;
                }
            }
        }

        // Increment the claims registered if drop is FC or Simple
        match &drop.drop_type {
            DropType::FunctionCall(data) => {
                drop.registered_uses += num_claims_per_key * len as u64;

                // If GAS is specified, set the GAS to attach for allowance calculations
                if let Some(_) = data.config.clone().and_then(|c| c.attached_gas) {
                    access_key_method_names = ACCESS_KEY_CLAIM_METHOD_NAME;
                }
            }
            DropType::Simple => {
                drop.registered_uses += num_claims_per_key * len as u64;
            }
            _ => {}
        };

        // Add the drop back in for the drop ID
        self.drop_for_id.insert(&drop_id, &drop);

        // Get the current balance of the funder.
        let mut current_user_balance = self
            .user_balances
            .get(&funder)
            .expect("No user balance found");
        near_sdk::log!("Cur user balance {}", yocto_to_near(current_user_balance));

        // Get the required attached_deposit for all the FCs
        let mut deposit_required_for_fc_deposits = 0;
        // Get the number of none FCs in FCData (if there are any)
        let mut num_none_fcs = 0;
        if let DropType::FunctionCall(data) = &drop.drop_type {
            // Ensure proper method data is passed in
            let num_method_data = data.clone().methods.len() as u64;

            // If there's one method data specified and more than 1 claim per key, that data is to be used
            // For all the claims. In this case, we need to tally all the deposits for each method in all method data.
            if num_claims_per_key > 1 && num_method_data == 1 {
                let attached_deposit = data
                    .methods
                    .iter()
                    .next()
                    .unwrap()
                    .clone()
                    .expect("cannot have a single none function call")
                    // iterate through   all entries and sum the attached_deposit
                    .iter()
                    .fold(0, |acc, x| acc + x.attached_deposit.0);

                near_sdk::log!(format!(
                    "Total attached_deposits for all method data: {}",
                    attached_deposit
                )
                .as_str());
                deposit_required_for_fc_deposits = num_claims_per_key as u128 * attached_deposit;
            // In the case where either there's 1 claim per key or the number of FCs is not 1,
            // We can simply loop through and manually get this data
            } else {
                for method_name in data.methods.clone() {
                    num_none_fcs += method_name.is_none() as u64;
                    // If the method is not None, we need to get the attached_deposit by looping through the method datas
                    if let Some(method_data) = method_name {
                        let attached_deposit = method_data
                            .iter()
                            .fold(0, |acc, x| acc + x.attached_deposit.0);
                        near_sdk::log!(
                            format!("Adding attached deposit: {}", attached_deposit).as_str()
                        );
                        deposit_required_for_fc_deposits += attached_deposit;
                    }
                }
            }
        }

        // Get optional costs
        let mut nft_optional_costs_per_key = 0;
        let mut ft_optional_costs_per_claim = 0;
        match drop.drop_type {
            DropType::NonFungibleToken(data) => {
                nft_optional_costs_per_key = data.storage_for_longest * env::storage_byte_cost()
            }
            DropType::FungibleToken(data) => ft_optional_costs_per_claim = data.ft_storage.0,
            _ => {}
        };

        // Calculate the storage being used for the entire drop
        let final_storage = env::storage_usage();
        let total_required_storage =
            Balance::from(final_storage - initial_storage) * env::storage_byte_cost();
        near_sdk::log!("Total required storage Yocto {}", total_required_storage);

        /*
            Required attached_deposit consists of:
            - Fees
            - TOTAL Storage
            - Total access key allowance for EACH key
            - Access key storage for EACH key
            - Balance for each key * (number of claims - claims with None for FC Data)

            Optional:
            - FC attached_deposit for each key * num Some(data) claims
            - storage for longest token ID for each key
            - FT storage registration cost for each key * claims (calculated in resolve storage calculation function)
        */
        let fees = self
            .fees_per_user
            .get(&funder)
            .unwrap_or((self.drop_fee, self.key_fee));
        let required_deposit = total_required_storage
            + (fees.1 // key fee
                + actual_allowance
                + ACCESS_KEY_STORAGE
                + drop.deposit_per_use * (num_claims_per_key - num_none_fcs) as u128
                + nft_optional_costs_per_key
                + deposit_required_for_fc_deposits
                + ft_optional_costs_per_claim * num_claims_per_key as u128)
                * len;

        near_sdk::log!(
            "Current balance: {}, 
            Required Deposit: {},  
            Total Required Storage: {}, 
            Key Fee: {}, 
            ACCESS_KEY_ALLOWANCE: {}, 
            ACCESS_KEY_STORAGE: {},
            Linkdrop Balance: {}, 
            NFT Optional costs per key: {},
            total function call deposits per key: {},
            FT Optional costs per claim: {},
            Num claims per key: {}
            Num none FCs: {},
            length: {}",
            yocto_to_near(current_user_balance),
            yocto_to_near(required_deposit),
            yocto_to_near(total_required_storage),
            yocto_to_near(fees.1),
            yocto_to_near(actual_allowance),
            yocto_to_near(ACCESS_KEY_STORAGE),
            yocto_to_near(drop.deposit_per_use),
            yocto_to_near(nft_optional_costs_per_key),
            yocto_to_near(deposit_required_for_fc_deposits),
            yocto_to_near(ft_optional_costs_per_claim),
            num_claims_per_key,
            num_none_fcs,
            len,
        );
        /*
            Ensure the attached attached_deposit can cover:
        */
        require!(
            current_user_balance >= required_deposit,
            "Not enough attached_deposit"
        );
        // Decrement the user's balance by the required attached_deposit and insert back into the map
        current_user_balance -= required_deposit;
        self.user_balances.insert(&funder, &current_user_balance);
        near_sdk::log!("New user balance {}", yocto_to_near(current_user_balance));

        // Increment our fees earned
        self.fees_collected += fees.1 * len;
        near_sdk::log!("Fees collected {}", yocto_to_near(fees.1 * len));

        // Create a new promise batch to create all the access keys
        let current_account_id = env::current_account_id();
        let promise = env::promise_batch_create(&current_account_id);

        // Loop through each public key and create the access keys
        for pk in public_keys.clone() {
            // Must assert in the loop so no access keys are made?
            env::promise_batch_action_add_key_with_function_call(
                promise,
                &pk,
                0,
                actual_allowance,
                &current_account_id,
                access_key_method_names,
            );
        }

        env::promise_return(promise);

        drop_id
    }
}
