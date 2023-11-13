// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::db_access_objects::payable_dao::PayableAccount;
use crate::accountant::payment_adjuster::miscellaneous::data_structures::AdjustedAccountBeforeFinalization;
use crate::masq_lib::utils::ExpectValue;
use crate::sub_lib::wallet::Wallet;
use itertools::Itertools;
use masq_lib::constants::WALLET_ADDRESS_LENGTH;
use masq_lib::logger::Logger;
use std::collections::HashMap;
use std::iter::once;
use std::ops::Not;
use thousands::Separable;
use web3::types::U256;

const REFILL_RECOMMENDATION: &str = "\
Please be aware that ignoring your debts might result in delinquency bans. In order to consume \
services without limitations, you will need to put more funds into your consuming wallet.";
const LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY: &str = "\
Passed successfully adjustment by transaction fee but noticing critical scarcity of MASQ balance. \
Operation will abort.";

const BLANK_SPACE: &str = "";

pub fn format_brief_adjustment_summary(
    original_account_balances_mapped: HashMap<Wallet, u128>,
    adjusted_accounts: &[PayableAccount],
) -> String {
    fn format_summary_for_included_accounts(
        original_account_balances_mapped: &HashMap<Wallet, u128>,
        adjusted_accounts: &[PayableAccount],
    ) -> String {
        adjusted_accounts
            .iter()
            .sorted_by(|account_a, account_b| {
                Ord::cmp(&account_b.balance_wei, &account_a.balance_wei)
            })
            .map(|account| {
                format!(
                    "{} {}\n{:^length$} {}",
                    account.wallet,
                    original_account_balances_mapped
                        .get(&account.wallet)
                        .expectv("initial balance"),
                    BLANK_SPACE,
                    account.balance_wei,
                    length = WALLET_ADDRESS_LENGTH
                )
            })
            .join("\n")
    }
    fn format_summary_for_excluded_accounts(excluded: &[(&Wallet, u128)]) -> String {
        let title = once(format!(
            "\n{:<length$} Original\n",
            "Ruled Out",
            length = WALLET_ADDRESS_LENGTH
        ));
        let list = excluded
            .iter()
            .sorted_by(|(_, balance_account_a), (_, balance_account_b)| {
                Ord::cmp(&balance_account_b, &balance_account_a)
            })
            .map(|(wallet, original_balance)| format!("{} {}", wallet, original_balance));
        title.chain(list).join("\n")
    }

    let adjusted_accounts_wallets: Vec<&Wallet> = adjusted_accounts
        .iter()
        .map(|account| &account.wallet)
        .collect();
    let excluded: Vec<(&Wallet, u128)> = original_account_balances_mapped.iter().fold(
        vec![],
        |mut acc, (wallet, original_balance)| {
            if !adjusted_accounts_wallets.contains(&wallet) {
                acc.push((wallet, *original_balance));
            }
            acc
        },
    );
    let adjusted_accounts_summary =
        format_summary_for_included_accounts(&original_account_balances_mapped, adjusted_accounts);
    let excluded_accounts_summary_opt = excluded
        .is_empty()
        .not()
        .then(|| format_summary_for_excluded_accounts(&excluded));
    vec![
        Some(adjusted_accounts_summary),
        excluded_accounts_summary_opt,
    ]
    .into_iter()
    .flatten()
    .join("\n")
}

const UNDERLINING_LENGTH: usize = 58;

pub fn before_and_after_debug_msg(
    original_account_balances_mapped: HashMap<Wallet, u128>,
    adjusted_accounts: &[PayableAccount],
) -> String {
    format!(
        "\n\
            {:<length$} {}\n\
            {}\n\
            {:<length$} {}\n\
            {:<length$} {}\n\
            \n\
            {}",
        "Payable Account",
        "Balance Wei",
        "-".repeat(UNDERLINING_LENGTH),
        "Successfully Adjusted",
        "Original",
        BLANK_SPACE,
        "Adjusted",
        format_brief_adjustment_summary(original_account_balances_mapped, adjusted_accounts),
        length = WALLET_ADDRESS_LENGTH
    )
}

pub fn info_log_for_disqualified_account(
    logger: &Logger,
    account: &AdjustedAccountBeforeFinalization,
) {
    info!(
        logger,
        "Shortage of MASQ in your consuming wallet impacts on payable {}, ruled out from this \
        round of payments. The proposed adjustment {} wei was less than half of the recorded \
        debt, {} wei",
        account.original_account.wallet,
        account.proposed_adjusted_balance.separate_with_commas(),
        account.original_account.balance_wei.separate_with_commas()
    )
}

pub fn log_adjustment_by_service_fee_is_required(
    logger: &Logger,
    payables_sum: u128,
    cw_masq_balance: u128,
) {
    warning!(
        logger,
        "Total of {} wei in MASQ was ordered while the consuming wallet held only {} wei of \
        the MASQ token. Adjustment in their count or the amounts is required.",
        payables_sum.separate_with_commas(),
        cw_masq_balance.separate_with_commas()
    );
    info!(logger, "{}", REFILL_RECOMMENDATION)
}

pub fn log_insufficient_transaction_fee_balance(
    logger: &Logger,
    required_transactions_count: u16,
    transaction_fee_minor: U256,
    limiting_count: u16,
) {
    warning!(
        logger,
        "Transaction fee amount {} wei from your wallet will not cover anticipated \
        fees to send {} transactions. Maximum is {}. The payments count needs to be \
        adjusted.",
        transaction_fee_minor.separate_with_commas(),
        required_transactions_count,
        limiting_count
    );
    info!(logger, "{}", REFILL_RECOMMENDATION)
}

pub fn log_transaction_fee_adjustment_ok_but_by_service_fee_undoable(logger: &Logger) {
    error!(logger, "{}", LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY)
}

#[cfg(test)]
mod tests {
    use crate::accountant::payment_adjuster::log_fns::{
        LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY, REFILL_RECOMMENDATION,
    };

    #[test]
    fn constants_are_correct() {
        assert_eq!(
            REFILL_RECOMMENDATION,
            "Please be aware that ignoring your debts might result in delinquency bans. In order to \
            consume services without limitations, you will need to put more funds into your \
            consuming wallet."
        );
        assert_eq!(
            LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY,
            "Passed successfully adjustment by transaction fee but noticing critical scarcity of \
            MASQ balance. Operation will abort."
        )
    }
}