// Copyright (c) 2019-2021, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

pub const HIGHEST_RANDOM_CLANDESTINE_PORT: u16 = 9999;
pub const HTTP_PORT: u16 = 80;
pub const TLS_PORT: u16 = 443;
pub const DEFAULT_CHAIN_NAME: &str = "mainnet"; //TODO look, this is different here and in the node's crate; there we call a default 'ropsten' which is quite concerning anyway
pub const DEFAULT_GAS_PRICE: u64 = 1;
pub const LOWEST_USABLE_INSECURE_PORT: u16 = 1025;
pub const HIGHEST_USABLE_PORT: u16 = 65535;
pub const DEFAULT_UI_PORT: u16 = 5333;
pub const CURRENT_LOGFILE_NAME: &str = "MASQNode_rCURRENT.log";

pub const MASQ_PROMPT: &str = "masq> ";

//error codes

//moved from configurator
pub const CONFIGURATOR_PREFIX: u64 = 0x0001_0000_0000_0000;
pub const CONFIGURATOR_READ_ERROR: u64 = CONFIGURATOR_PREFIX | 1;
pub const CONFIGURATOR_WRITE_ERROR: u64 = CONFIGURATOR_PREFIX | 2;
pub const UNRECOGNIZED_MNEMONIC_LANGUAGE_ERROR: u64 = CONFIGURATOR_PREFIX | 3;
pub const ILLEGAL_MNEMONIC_WORD_COUNT_ERROR: u64 = CONFIGURATOR_PREFIX | 4;
pub const KEY_PAIR_CONSTRUCTION_ERROR: u64 = CONFIGURATOR_PREFIX | 5;
pub const BAD_PASSWORD_ERROR: u64 = CONFIGURATOR_PREFIX | 6;
pub const ALREADY_INITIALIZED_ERROR: u64 = CONFIGURATOR_PREFIX | 7;
pub const DERIVATION_PATH_ERROR: u64 = CONFIGURATOR_PREFIX | 8;
pub const MNEMONIC_PHRASE_ERROR: u64 = CONFIGURATOR_PREFIX | 9;
pub const EARLY_QUESTIONING_ABOUT_DATA: u64 = CONFIGURATOR_PREFIX | 10;
pub const UNRECOGNIZED_PARAMETER: u64 = CONFIGURATOR_PREFIX | 11;
pub const NON_PARSABLE_VALUE: u64 = CONFIGURATOR_PREFIX | 12;

//moved from masq_lib/messages
pub const UI_NODE_COMMUNICATION_PREFIX: u64 = 0x8000_0000_0000_0000;
pub const NODE_LAUNCH_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 1;
pub const NODE_NOT_RUNNING_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 2;
pub const NODE_ALREADY_RUNNING_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 3;
pub const UNMARSHAL_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 4;
pub const SETUP_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 5;
pub const TIMEOUT_ERROR: u64 = UI_NODE_COMMUNICATION_PREFIX | 6;
