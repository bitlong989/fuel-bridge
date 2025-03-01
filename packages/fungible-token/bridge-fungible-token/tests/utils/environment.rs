use crate::utils::builder;

use std::{mem::size_of, num::ParseIntError, result::Result as StdResult, str::FromStr};

use fuel_core_types::{
    fuel_tx::{Bytes32, Input, Output, Receipt, TxPointer, UtxoId},
    fuel_types::Word,
};
use fuels::{
    accounts::{
        fuel_crypto::{fuel_types::Nonce, SecretKey},
        predicate::Predicate,
        wallet::WalletUnlocked,
        Signer, ViewOnlyAccount,
    },
    prelude::{
        abigen, setup_custom_assets_coins, setup_test_provider, Address, AssetConfig, AssetId,
        Bech32ContractId, Config, Contract, ContractId, LoadConfiguration, Provider,
        ScriptTransaction, TxParameters,
    },
    test_helpers::{setup_single_message, DEFAULT_COIN_AMOUNT},
    types::{message::Message, Bits256},
};
use primitive_types::U256 as Unsigned256;
use sha3::{Digest, Keccak256};

const CONTRACT_MESSAGE_PREDICATE_BINARY: &str =
    "../bridge-message-predicates/contract_message_predicate.bin";
const MESSAGE_SENDER_ADDRESS: &str =
    "0x00000000000000000000000096c53cd98B7297564716a8f2E1de2C83928Af2fe";
const TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY: &str =
    "../bridge-fungible-token/out/debug/bridge_fungible_token.bin";
const DEPOSIT_RECIPIENT_CONTRACT_BINARY: &str =
    "../test-deposit-recipient-contract/out/debug/test_deposit_recipient_contract.bin";

abigen!(
    Contract(
        name = "BridgeFungibleTokenContract",
        abi = "./bridge-fungible-token/out/debug/bridge_fungible_token-abi.json",
    ),
    Predicate(
        name = "ContractMessagePredicate",
        abi = "./bridge-message-predicates/contract_message_predicate-abi.json"
    ),
    Script(
        name = "ContractMessageScript",
        abi = "./bridge-message-predicates/contract_message_script-abi.json",
    ),
    Contract(
        name = "DepositRecipientContract",
        abi =
            "./test-deposit-recipient-contract/out/debug/test_deposit_recipient_contract-abi.json",
    ),
);

pub struct TestConfig {
    pub adjustment_factor: Unsigned256,
    pub adjustment_is_div: bool,
    pub min_amount: Unsigned256,
    pub max_amount: Unsigned256,
    pub test_amount: Unsigned256,
    pub not_enough: Unsigned256,
    pub overflow_1: Unsigned256,
    pub overflow_2: Unsigned256,
    pub overflow_3: Unsigned256,
}

impl TestConfig {
    pub fn fuel_equivalent_amount(&self, amount: Unsigned256) -> u64 {
        if self.adjustment_is_div {
            (amount * self.adjustment_factor).as_u64()
        } else {
            (amount / self.adjustment_factor).as_u64()
        }
    }
}

fn keccak_hash<B>(data: B) -> Bytes32
where
    B: AsRef<[u8]>,
{
    let mut hasher = Keccak256::new();
    hasher.update(data);
    <[u8; Bytes32::LEN]>::from(hasher.finalize()).into()
}

pub fn generate_test_config(decimals: (u8, u8)) -> TestConfig {
    let bridged_token_decimals = Unsigned256::from(decimals.0);
    let proxy_token_decimals = Unsigned256::from(decimals.1);
    let one = Unsigned256::from(1);

    let adjustment_factor = match (bridged_token_decimals, proxy_token_decimals) {
        (bridged_token_decimals, proxy_token_decimals)
            if bridged_token_decimals > proxy_token_decimals =>
        {
            Unsigned256::from(10).pow(bridged_token_decimals - proxy_token_decimals)
        }
        (bridged_token_decimals, proxy_token_decimals)
            if bridged_token_decimals < proxy_token_decimals =>
        {
            Unsigned256::from(10).pow(proxy_token_decimals - bridged_token_decimals)
        }
        _ => one,
    };

    let adjustment_is_div = bridged_token_decimals < proxy_token_decimals;

    let min_amount = if bridged_token_decimals > proxy_token_decimals {
        Unsigned256::from(1) * adjustment_factor
    } else {
        one
    };

    let max_amount = match (bridged_token_decimals, proxy_token_decimals) {
        (bridged_token_decimals, proxy_token_decimals)
            if bridged_token_decimals > proxy_token_decimals =>
        {
            Unsigned256::from(u64::MAX) * adjustment_factor
        }
        (bridged_token_decimals, proxy_token_decimals)
            if bridged_token_decimals < proxy_token_decimals =>
        {
            Unsigned256::from(u64::MAX) / adjustment_factor
        }
        (_, _) => one,
    };

    let test_amount = (min_amount + max_amount) / Unsigned256::from(2);
    let not_enough = min_amount - one;
    let overflow_1 = max_amount + one;
    let overflow_2 = max_amount + (one << 160);
    let overflow_3 = max_amount + (one << 224);

    TestConfig {
        adjustment_factor,
        adjustment_is_div,
        min_amount,
        test_amount,
        max_amount,
        not_enough,
        overflow_1,
        overflow_2,
        overflow_3,
    }
}

pub fn setup_wallet() -> WalletUnlocked {
    // Create secret for wallet
    const SIZE_SECRET_KEY: usize = size_of::<SecretKey>();
    const PADDING_BYTES: usize = SIZE_SECRET_KEY - size_of::<u64>();
    let mut secret_key: [u8; SIZE_SECRET_KEY] = [0; SIZE_SECRET_KEY];
    secret_key[PADDING_BYTES..].copy_from_slice(&(8320147306839812359u64).to_be_bytes());

    // Generate wallet
    let wallet = WalletUnlocked::new_from_private_key(
        SecretKey::try_from(secret_key.as_slice()).unwrap(),
        None,
    );
    wallet
}

/// Sets up a test fuel environment with a funded wallet
pub async fn setup_environment(
    wallet: &mut WalletUnlocked,
    coins: Vec<(Word, AssetId)>,
    messages: Vec<(Word, Vec<u8>)>,
    deposit_contract: Option<ContractId>,
    sender: Option<&str>,
    configurables: Option<BridgeFungibleTokenContractConfigurables>,
) -> (
    BridgeFungibleTokenContract<WalletUnlocked>,
    Vec<Input>,
    Vec<Input>,
    Vec<Input>,
    Bech32ContractId,
    Provider,
) {
    // Generate coins for wallet
    let asset_configs: Vec<AssetConfig> = coins
        .iter()
        .map(|coin| AssetConfig {
            id: coin.1,
            num_coins: 1,
            coin_amount: coin.0,
        })
        .collect();
    let all_coins = setup_custom_assets_coins(wallet.address(), &asset_configs[..]);

    // Generate message
    let mut message_nonce = Nonce::zeroed();
    let message_sender = match sender {
        Some(v) => Address::from_str(v).unwrap(),
        None => Address::from_str(MESSAGE_SENDER_ADDRESS).unwrap(),
    };

    let predicate = Predicate::load_from(CONTRACT_MESSAGE_PREDICATE_BINARY).unwrap();
    let predicate_root = predicate.address();

    let mut all_messages: Vec<Message> = vec![];
    for msg in messages {
        all_messages.push(setup_single_message(
            &message_sender.into(),
            predicate_root,
            msg.0,
            message_nonce,
            msg.1.clone(),
        ));
        message_nonce[0] += 1;
    }

    let (provider, _) = setup_test_provider(
        all_coins.clone(),
        all_messages.clone(),
        Some(Config::local_node()),
        None,
    )
    .await;

    wallet.set_provider(provider.clone());

    let test_contract_id = match configurables {
        Some(config) => Contract::load_from(
            TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY,
            LoadConfiguration::default().set_configurables(config),
        )
        .unwrap()
        .deploy(&wallet.clone(), TxParameters::default())
        .await
        .unwrap(),
        None => Contract::load_from(
            TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY,
            LoadConfiguration::default(),
        )
        .unwrap()
        .deploy(&wallet.clone(), TxParameters::default())
        .await
        .unwrap(),
    };

    let test_contract = BridgeFungibleTokenContract::new(test_contract_id.clone(), wallet.clone());

    // Build inputs for provided coins
    let coin_inputs = all_coins
        .into_iter()
        .map(|coin| {
            Input::coin_signed(
                coin.utxo_id,
                coin.owner.into(),
                coin.amount,
                coin.asset_id,
                Default::default(),
                0,
                coin.maturity.into(),
            )
        })
        .collect();

    // Build inputs for provided messages
    let message_inputs = all_messages
        .into_iter()
        .map(|message| {
            if message.data.is_empty() {
                Input::message_coin_predicate(
                    message.sender.into(),
                    message.recipient.into(),
                    message.amount,
                    message.nonce,
                    predicate.code().to_vec(),
                    vec![],
                )
            } else {
                Input::message_data_predicate(
                    message.sender.into(),
                    message.recipient.into(),
                    message.amount,
                    message.nonce,
                    message.data,
                    predicate.code().to_vec(),
                    vec![],
                )
            }
        })
        .collect();

    // Build contract inputs
    let mut contract_inputs = vec![Input::contract(
        UtxoId::new(Bytes32::zeroed(), 0u8),
        Bytes32::zeroed(),
        Bytes32::zeroed(),
        TxPointer::default(),
        test_contract_id.clone().into(),
    )];

    if let Some(id) = deposit_contract {
        contract_inputs.push(Input::contract(
            UtxoId::new(Bytes32::zeroed(), 0u8),
            Bytes32::zeroed(),
            Bytes32::zeroed(),
            TxPointer::default(),
            id,
        ));
    }

    (
        test_contract,
        contract_inputs,
        coin_inputs,
        message_inputs,
        test_contract_id,
        provider,
    )
}

/// Relays a message-to-contract message
pub async fn relay_message_to_contract(
    wallet: &WalletUnlocked,
    message: Input,
    contracts: Vec<Input>,
    gas_coins: &[Input],
    optional_outputs: &[Output],
) -> Vec<Receipt> {
    // Build transaction
    let mut tx = builder::build_contract_message_tx(
        message,
        contracts,
        gas_coins,
        optional_outputs,
        TxParameters::default(),
    )
    .await;

    // Sign transaction and call
    sign_and_call_tx(wallet, &mut tx).await
}

/// Relays a message-to-contract message
pub async fn sign_and_call_tx(wallet: &WalletUnlocked, tx: &mut ScriptTransaction) -> Vec<Receipt> {
    // Get provider and client
    let provider = wallet.provider().unwrap();

    // Sign transaction and call
    wallet.sign_transaction(tx).unwrap();
    provider.send_transaction(tx).await.unwrap()
}

pub async fn precalculate_deposit_id() -> ContractId {
    let compiled = Contract::load_from(
        DEPOSIT_RECIPIENT_CONTRACT_BINARY,
        LoadConfiguration::default(),
    )
    .unwrap();

    compiled.contract_id()
}

/// Prefixes the given bytes with the test contract ID
pub async fn prefix_contract_id(
    mut data: Vec<u8>,
    config: Option<BridgeFungibleTokenContractConfigurables>,
) -> Vec<u8> {
    // Compute the test contract ID
    let compiled_contract = match config {
        Some(c) => Contract::load_from(
            TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY,
            LoadConfiguration::default().set_configurables(c),
        )
        .unwrap(),
        None => Contract::load_from(
            TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY,
            LoadConfiguration::default(),
        )
        .unwrap(),
    };

    let test_contract_id = compiled_contract.contract_id();

    // Turn contract id into array with the given data appended to it
    let test_contract_id: [u8; 32] = test_contract_id.into();
    let mut test_contract_id = test_contract_id.to_vec();
    test_contract_id.append(&mut data);
    test_contract_id
}

/// Quickly converts the given hex string into a u8 vector
pub fn decode_hex(s: &str) -> Vec<u8> {
    let data: StdResult<Vec<u8>, ParseIntError> = (2..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect();
    data.unwrap()
}

pub async fn get_fungible_token_instance(
    wallet: WalletUnlocked,
) -> (BridgeFungibleTokenContract<WalletUnlocked>, ContractId) {
    // Deploy the target contract used for testing processing messages
    let fungible_token_contract_id = Contract::load_from(
        TEST_BRIDGE_FUNGIBLE_TOKEN_CONTRACT_BINARY,
        LoadConfiguration::default(),
    )
    .unwrap()
    .deploy(&wallet, TxParameters::default())
    .await
    .unwrap();

    let fungible_token_instance =
        BridgeFungibleTokenContract::new(fungible_token_contract_id.clone(), wallet);

    (fungible_token_instance, fungible_token_contract_id.into())
}

pub async fn get_deposit_recipient_contract_instance(
    wallet: WalletUnlocked,
) -> (DepositRecipientContract<WalletUnlocked>, ContractId) {
    // Deploy the target contract used for testing processing messages
    let deposit_recipient_contract_id = Contract::load_from(
        DEPOSIT_RECIPIENT_CONTRACT_BINARY,
        LoadConfiguration::default(),
    )
    .unwrap()
    .deploy(&wallet, TxParameters::default())
    .await
    .unwrap();

    let deposit_recipient_contract =
        DepositRecipientContract::new(deposit_recipient_contract_id.clone(), wallet);

    (
        deposit_recipient_contract,
        deposit_recipient_contract_id.into(),
    )
}

pub fn encode_hex(val: Unsigned256) -> [u8; 32] {
    let mut arr = [0u8; 32];
    val.to_big_endian(&mut arr);
    arr
}

pub async fn construct_msg_data(
    token: &str,
    from: &str,
    to: [u8; 32],
    amount: Unsigned256,
    config: Option<BridgeFungibleTokenContractConfigurables>,
    deposit_to_contract: bool,
    // TODO: https://github.com/FuelLabs/bridge-fungible-token/issues/61
    extra_data: Option<Vec<u8>>,
) -> ((u64, Vec<u8>), (u64, AssetId), Option<ContractId>) {
    let mut message_data = Vec::with_capacity(5);
    message_data.append(&mut decode_hex(token));
    message_data.append(&mut decode_hex(from));
    message_data.append(&mut to.to_vec());
    message_data.append(&mut encode_hex(amount).to_vec());

    let mut deposit_recipient: Option<ContractId> = None;

    if deposit_to_contract {
        let hash = keccak_hash("DEPOSIT_TO_CONTRACT");
        let mut byte: Vec<u8> = vec![0u8];
        byte.copy_from_slice(&hash[..1]);
        message_data.append(&mut byte);
        deposit_recipient = Option::Some(ContractId::new(to));
    };

    if let Some(mut data) = extra_data {
        message_data.append(&mut data);
    };

    let message_data = prefix_contract_id(message_data, config).await;
    let message = (100, message_data);
    let coin = (DEFAULT_COIN_AMOUNT, AssetId::default());

    (message, coin, deposit_recipient)
}

pub fn generate_variable_output() -> Vec<Output> {
    vec![Output::variable(Address::zeroed(), 0, AssetId::default())]
}

pub fn parse_output_message_data(data: &[u8]) -> (Vec<u8>, Bits256, Bits256, Unsigned256) {
    let selector = &data[0..4];
    let to: [u8; 32] = data[4..36].try_into().unwrap();
    let token_array: [u8; 32] = data[36..68].try_into().unwrap();
    let token = Bits256(token_array);
    let amount_array: [u8; 32] = data[68..100].try_into().unwrap();
    let amount: Unsigned256 = Unsigned256::from_big_endian(amount_array.as_ref());
    (selector.to_vec(), Bits256(to), token, amount)
}
