// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
use crate::sub_lib::wallet::Wallet;
use ethereum_types::U256;
use masq_lib::logger::Logger;
use masq_lib::percentage::PurePercentage;

#[derive(Clone)]
pub struct BlockchainAgentNull {
    wallet: Wallet,
    logger: Logger,
}

impl BlockchainAgent for BlockchainAgentNull {
    fn estimated_transaction_fee_per_transaction_minor(&self) -> u128 {
        self.log_function_call("estimated_transaction_fee_per_transaction_minor()");
        0
    }

    fn transaction_fee_balance_minor(&self) -> U256 {
        self.log_function_call("transaction_fee_balance_minor()");
        U256::zero()
    }

    fn service_fee_balance_minor(&self) -> u128 {
        self.log_function_call("service_fee_balance_minor()");
        0
    }

    fn gas_price(&self) -> u64 {
        self.log_function_call("gas_price()");
        0
    }

    fn gas_price_margin(&self) -> PurePercentage {
        self.log_function_call("gas_price_margin()");
        PurePercentage::try_from(0).expect("0 should cause no issue")
    }

    fn consuming_wallet(&self) -> &Wallet {
        self.log_function_call("consuming_wallet()");
        &self.wallet
    }

    fn pending_transaction_id(&self) -> U256 {
        self.log_function_call("pending_transaction_id()");
        U256::zero()
    }

    #[cfg(test)]
    fn dup(&self) -> Box<dyn BlockchainAgent> {
        intentionally_blank!()
    }

    #[cfg(test)]
    as_any_ref_in_trait_impl!();
}

impl BlockchainAgentNull {
    pub fn new() -> Self {
        Self {
            wallet: Wallet::null(),
            logger: Logger::new("BlockchainAgentNull"),
        }
    }

    fn log_function_call(&self, function_call: &str) {
        error!(
            self.logger,
            "calling null version of {function_call} for BlockchainAgentNull will be without effect",
        );
    }
}

impl Default for BlockchainAgentNull {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::agent_null::BlockchainAgentNull;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
    use crate::sub_lib::wallet::Wallet;
    use masq_lib::logger::Logger;
    use masq_lib::percentage::PurePercentage;
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use web3::types::U256;

    fn blockchain_agent_null_constructor_works<C>(constructor: C)
    where
        C: Fn() -> BlockchainAgentNull,
    {
        init_test_logging();

        let result = constructor();

        assert_eq!(result.wallet, Wallet::null());
        warning!(result.logger, "blockchain_agent_null_constructor_works");
        TestLogHandler::default().exists_log_containing(
            "WARN: BlockchainAgentNull: \
        blockchain_agent_null_constructor_works",
        );
    }

    #[test]
    fn blockchain_agent_null_constructor_works_for_new() {
        blockchain_agent_null_constructor_works(BlockchainAgentNull::new)
    }

    #[test]
    fn blockchain_agent_null_constructor_works_for_default() {
        blockchain_agent_null_constructor_works(BlockchainAgentNull::default)
    }

    fn assert_error_log(test_name: &str, expected_operation: &str) {
        TestLogHandler::default().exists_log_containing(&format!(
            "ERROR: {test_name}: calling \
            null version of {expected_operation}() for BlockchainAgentNull \
            will be without effect"
        ));
    }

    #[test]
    fn null_agent_estimated_transaction_fee_per_transaction_minor() {
        init_test_logging();
        let test_name = "null_agent_estimated_transaction_fee_per_transaction_minor";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.estimated_transaction_fee_per_transaction_minor();

        assert_eq!(result, 0);
        assert_error_log(test_name, "estimated_transaction_fee_per_transaction_minor");
    }

    #[test]
    fn null_agent_consuming_transaction_fee_balance_minor() {
        init_test_logging();
        let test_name = "null_agent_consuming_transaction_fee_balance_minor";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.transaction_fee_balance_minor();

        assert_eq!(result, U256::zero());
        assert_error_log(test_name, "transaction_fee_balance_minor")
    }

    #[test]
    fn null_agent_service_fee_balance_minor() {
        init_test_logging();
        let test_name = "null_agent_service_fee_balance_minor";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.service_fee_balance_minor();

        assert_eq!(result, 0);
        assert_error_log(test_name, "service_fee_balance_minor")
    }

    #[test]
    fn null_agent_gas_price() {
        init_test_logging();
        let test_name = "null_agent_gas_price";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.gas_price();

        assert_eq!(result, 0);
        assert_error_log(test_name, "gas_price")
    }

    #[test]
    fn null_agent_gas_price_margin() {
        init_test_logging();
        let test_name = "null_agent_gas_price_margin";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.gas_price_margin();

        assert_eq!(result, PurePercentage::try_from(0).unwrap());
        assert_error_log(test_name, "gas_price_margin")
    }

    #[test]
    fn null_agent_consuming_wallet() {
        init_test_logging();
        let test_name = "null_agent_consuming_wallet";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.consuming_wallet();

        assert_eq!(result, &Wallet::null());
        assert_error_log(test_name, "consuming_wallet")
    }

    #[test]
    fn null_agent_pending_transaction_id() {
        init_test_logging();
        let test_name = "null_agent_pending_transaction_id";
        let mut subject = BlockchainAgentNull::new();
        subject.logger = Logger::new(test_name);

        let result = subject.pending_transaction_id();

        assert_eq!(result, U256::zero());
        assert_error_log(test_name, "pending_transaction_id");
    }
}
