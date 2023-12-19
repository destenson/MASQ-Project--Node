// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::checked_conversion;
use crate::accountant::db_access_objects::receivable_dao::ReceivableDaoError;
use crate::accountant::db_big_integer::big_int_divider::BigIntDivider;
use crate::accountant::PayableDaoError;
use crate::database::rusqlite_wrappers::{ConnectionWrapper, SecureTransactionWrapper};
use crate::sub_lib::wallet::Wallet;
use itertools::Either;
use rusqlite::{Error, Row, Statement, ToSql};
use std::fmt::{Debug, Display, Formatter};
use std::iter::once;
use std::marker::PhantomData;
use std::ops::Neg;

pub trait BigIntDbProcessor<T>: Debug + Send
where
    T: TableNameDAO,
{
    fn execute<'params>(
        &self,
        conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
        config: BigIntSqlConfig<'params, T>,
    ) -> Result<(), BigIntDatabaseError>;
}

#[derive(Debug)]
pub struct BigIntDbProcessorReal<T: TableNameDAO> {
    overflow_handler: Box<dyn UpdateOverflowHandler<T>>,
}

impl<T> BigIntDbProcessor<T> for BigIntDbProcessorReal<T>
where
    T: TableNameDAO,
{
    fn execute<'params>(
        &self,
        conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
        config: BigIntSqlConfig<'params, T>,
    ) -> Result<(), BigIntDatabaseError> {
        let main_sql = config.main_sql;
        let mut stm = Self::prepare_statement(conn, main_sql);
        let params = config
            .params
            .merge_other_and_wei_params((&config.params.wei_change_params).into());
        match stm.execute(params.as_slice()) {
            Ok(1) => Ok(()),
            Ok(x) => Err(BigIntDatabaseError::RowChangeMismatch{row_key: config.key_param_value().to_string(),detected_count_changed: x}),
            //SQLITE_CONSTRAINT_DATATYPE (3091),
            //the moment of Sqlite trying to store the number as REAL in a strict INT column
            Err(Error::SqliteFailure(e, _)) if e.extended_code == 3091 => {
                self.overflow_handler.update_with_overflow(conn, config)
            }
            Err(e) => Err(BigIntDatabaseError::General(format!(
                "Error from invalid {} command for {} table and change of {} wei to '{} = {}' with error '{}'",
                config.determine_command(),
                T::table_name(),
                config.balance_change(),
                config.params.table_unique_key_name,
                config.key_param_value(),
                e
            ))),
        }
    }
}

impl<T: TableNameDAO + 'static> Default for BigIntDbProcessorReal<T> {
    fn default() -> BigIntDbProcessorReal<T> {
        Self {
            overflow_handler: Box::new(UpdateOverflowHandlerReal::default()),
        }
    }
}

impl<T: TableNameDAO> BigIntDbProcessorReal<T> {
    fn prepare_statement<'params>(
        form_of_conn: Either<&'params dyn ConnectionWrapper, &'params SecureTransactionWrapper>,
        sql: &'params str,
    ) -> Statement<'params> {
        match form_of_conn {
            Either::Left(conn) => conn.prepare(sql),
            Either::Right(tx) => tx.prepare(sql),
        }
        .expect("internal rusqlite error")
    }
}

pub trait UpdateOverflowHandler<T>: Debug + Send
where
    T: TableNameDAO,
{
    fn update_with_overflow<'params>(
        &self,
        conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
        config: BigIntSqlConfig<'params, T>,
    ) -> Result<(), BigIntDatabaseError>;
}

#[derive(Debug)]
struct UpdateOverflowHandlerReal<T: TableNameDAO> {
    phantom: PhantomData<T>,
}

impl<T: TableNameDAO> Default for UpdateOverflowHandlerReal<T> {
    fn default() -> Self {
        Self {
            phantom: Default::default(),
        }
    }
}

impl<T: TableNameDAO> UpdateOverflowHandler<T> for UpdateOverflowHandlerReal<T> {
    fn update_with_overflow<'params>(
        &self,
        conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
        config: BigIntSqlConfig<'params, T>,
    ) -> Result<(), BigIntDatabaseError> {
        let update_divided_integer = |row: &Row| -> Result<(), rusqlite::Error> {
            let high_bytes_result = row.get::<usize, i64>(0);
            let low_bytes_result = row.get::<usize, i64>(1);

            match [high_bytes_result, low_bytes_result] {
                [Ok(former_high_bytes), Ok(former_low_bytes)] => {
                    let requested_wei_change = &config.params.wei_change_params;
                    let (high_bytes_corrected, low_bytes_corrected) = Self::correct_bytes(
                        former_high_bytes,
                        former_low_bytes,
                        requested_wei_change,
                    );
                    let wei_update_array: [(&str, &dyn ToSql); 2] = [
                        (
                            requested_wei_change.high.name.as_str(),
                            &high_bytes_corrected,
                        ),
                        (requested_wei_change.low.name.as_str(), &low_bytes_corrected),
                    ];

                    let execute_params = config
                        .params
                        .merge_other_and_wei_params_with_conditional_participants(wei_update_array);

                    Self::execute_update(conn, &config, &execute_params);
                    Ok(())
                }
                array_of_results => Self::return_first_error(array_of_results),
            }
        };

        let select_sql = config.select_sql();
        let mut select_stm = BigIntDbProcessorReal::<T>::prepare_statement(conn, &select_sql);
        match select_stm.query_row([], update_divided_integer) {
            Ok(()) => Ok(()),
            Err(e) => Err(BigIntDatabaseError::General(format!(
                "Updating balance for {} table and change of {} wei to '{} = {}' with error '{}'",
                T::table_name(),
                config.balance_change(),
                config.params.table_unique_key_name,
                config.key_param_value(),
                e
            ))),
        }
    }
}

impl<T: TableNameDAO + Debug> UpdateOverflowHandlerReal<T> {
    fn execute_update<'params>(
        conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
        config: &BigIntSqlConfig<'params, T>,
        sql_params: &[(&str, &dyn ToSql)],
    ) {
        match BigIntDbProcessorReal::<T>::prepare_statement(conn, config.overflow_update_clause)
            .execute(sql_params)
            .expect("logic broken given the previous non-overflow call accepted right")
        {
            1 => (),
            x => panic!(
                "Broken code: this code was written to handle one changed row a time, not {}",
                x
            ),
        }
    }

    fn correct_bytes(
        former_high_bytes: i64,
        former_low_bytes: i64,
        requested_wei_change: &WeiChangeAsHighAndLowBytes,
    ) -> (i64, i64) {
        let high_bytes_correction = former_high_bytes + requested_wei_change.high.value + 1;
        let low_bytes_correction = ((former_low_bytes as i128
            + requested_wei_change.low.value as i128)
            & 0x7FFFFFFFFFFFFFFF) as i64;
        (high_bytes_correction, low_bytes_correction)
    }

    fn return_first_error(two_results: [rusqlite::Result<i64>; 2]) -> rusqlite::Result<()> {
        let cached = format!("{:?}", two_results);
        match two_results.into_iter().find(|result| result.is_err()) {
            Some(err) => Err(err.expect_err("we just said it is an error")),
            None => panic!(
                "Broken code: being called to process an error but none was found in {}",
                cached
            ),
        }
    }
}

pub struct BigIntSqlConfig<'params, T: TableNameDAO> {
    main_sql: &'params str,
    overflow_update_clause: &'params str,
    params: SQLParams<'params>,
    phantom: PhantomData<T>,
}

impl<'params, T: TableNameDAO> BigIntSqlConfig<'params, T> {
    pub fn new(
        main_sql: &'params str,
        overflow_update_clause: &'params str,
        params: SQLParams<'params>,
    ) -> BigIntSqlConfig<'params, T> {
        Self {
            main_sql,
            overflow_update_clause,
            params,
            phantom: Default::default(),
        }
    }

    fn select_sql(&self) -> String {
        format!(
            "select {}, {} from {} where {} = '{}'",
            &self.params.wei_change_params.high.name[1..],
            &self.params.wei_change_params.low.name[1..],
            T::table_name(),
            self.params.table_unique_key_name,
            self.key_param_value()
        )
    }

    fn key_param_value(&self) -> &'params dyn DisplayableParamValue {
        self.params.params_except_wei_change[0].param_pair().1
    }

    fn balance_change(&self) -> i128 {
        let wei_params = &self.params.wei_change_params;
        BigIntDivider::reconstitute(wei_params.high.value, wei_params.low.value)
    }

    fn determine_command(&self) -> String {
        let keyword = self
            .main_sql
            .chars()
            .skip_while(|char| char.is_whitespace())
            .take_while(|char| !char.is_whitespace())
            .collect::<String>();
        match keyword.as_str() {
            "insert" => {
                if self.main_sql.contains("update") {
                    "upsert".to_string()
                } else {
                    panic!("Sql with simple insert. The processor of big integers is correctly used only if combined with update")
                }
            }
            "update" => keyword,
            _ => panic!(
                "broken code: unexpected or misplaced command \"{}\" \
                 in upsert or update, respectively",
                keyword
            ),
        }
    }
}

// This is a native Rusqlite format for data supply to their interface
type RusqliteParamPair<'params> = (&'params str, &'params dyn ToSql);
// This is a representation of the above, plus providing a way to display the values (not possible natively)
type DisplayableRusqliteParamPair<'params> = (&'params str, &'params dyn DisplayableParamValue);

// To be able to display things that implement ToSql
pub trait DisplayableParamValue: ToSql + Display {}

macro_rules! implement_displayable_param_value {
    ($($implementer: ty),+) => {
        $(impl DisplayableParamValue for $implementer {})+
    }
}

implement_displayable_param_value!(i64, &str, Wallet);

#[derive(Default)]
pub struct SQLParamsBuilder<'params> {
    key_spec_opt: Option<TableUniqueKey<'params>>,
    wei_change_spec_opt: Option<WeiChange>,
    other_params: Vec<Param<'params>>,
}

impl<'params> SQLParamsBuilder<'params> {
    pub fn key(mut self, key_variant: KeyVariants<'params>) -> Self {
        let (definition_name, substitution_name_in_sql, value_itself) = key_variant.into();
        self.key_spec_opt = Some(TableUniqueKey {
            definition_name,
            substitution_name_in_sql,
            value_itself,
        });
        self
    }

    pub fn wei_change(mut self, wei_change: WeiChange) -> Self {
        self.wei_change_spec_opt = Some(wei_change);
        self
    }

    pub fn other_params(mut self, params: Vec<Param<'params>>) -> Self {
        self.other_params = params;
        self
    }

    pub fn build(self) -> SQLParams<'params> {
        let key_spec = self
            .key_spec_opt
            .unwrap_or_else(|| panic!("SQLparams cannot miss the component of a key"));
        let wei_change_spec = self
            .wei_change_spec_opt
            .unwrap_or_else(|| panic!("SQLparams cannot miss the component of wei change"));
        let ((high_bytes_param_name, low_bytes_param_name), (high_bytes_value, low_bytes_value)) =
            Self::expand_wei_params(wei_change_spec);
        let key_as_the_first_param = (key_spec.substitution_name_in_sql, key_spec.value_itself);
        let params = once(Param::BothClauses(key_as_the_first_param))
            .chain(self.other_params.into_iter())
            .collect();
        SQLParams {
            table_unique_key_name: key_spec.definition_name,
            wei_change_params: WeiChangeAsHighAndLowBytes {
                high: StdNumParamFormNamed::new(high_bytes_param_name, high_bytes_value),
                low: StdNumParamFormNamed::new(low_bytes_param_name, low_bytes_value),
            },
            params_except_wei_change: params,
        }
    }

    fn expand_wei_params(wei_change_spec: WeiChange) -> ((String, String), (i64, i64)) {
        let (name, num): (&'static str, i128) = match wei_change_spec {
            WeiChange::Addition(name, num) => (name, checked_conversion::<u128, i128>(num)),
            WeiChange::Subtraction(name, num) => {
                (name, checked_conversion::<u128, i128>(num).neg())
            }
        };
        let (high_bytes, low_bytes) = BigIntDivider::deconstruct(num);
        let param_sub_name_for_high_bytes =
            Self::proper_wei_change_param_name(name, ByteMagnitude::High);
        let param_sub_name_for_low_bytes =
            Self::proper_wei_change_param_name(name, ByteMagnitude::Low);
        (
            (param_sub_name_for_high_bytes, param_sub_name_for_low_bytes),
            (high_bytes, low_bytes),
        )
    }

    fn proper_wei_change_param_name(base_word: &str, byte_magnitude: ByteMagnitude) -> String {
        format!(":{}_{}_b", base_word, byte_magnitude)
    }
}

struct TableUniqueKey<'params> {
    definition_name: &'params str,
    substitution_name_in_sql: &'params str,
    value_itself: &'params dyn DisplayableParamValue,
}

pub enum KeyVariants<'params> {
    WalletAddress(&'params dyn DisplayableParamValue),
    PendingPayableRowid(&'params dyn DisplayableParamValue),
    #[cfg(test)]
    TestKey {
        var_name: &'static str,
        sub_name: &'static str,
        val: &'params dyn DisplayableParamValue,
    },
}

impl<'params> From<KeyVariants<'params>>
    for (
        &'static str,
        &'static str,
        &'params dyn DisplayableParamValue,
    )
{
    fn from(variant: KeyVariants<'params>) -> Self {
        match variant {
            KeyVariants::WalletAddress(val) => ("wallet_address", ":wallet", val),
            KeyVariants::PendingPayableRowid(val) => ("pending_payable_rowid", ":rowid", val),
            #[cfg(test)]
            KeyVariants::TestKey {
                var_name,
                sub_name,
                val,
            } => (var_name, sub_name, val),
        }
    }
}

enum ByteMagnitude {
    High,
    Low,
}

impl Display for ByteMagnitude {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::High => write!(f, "high"),
            Self::Low => write!(f, "low"),
        }
    }
}

pub struct SQLParams<'params> {
    table_unique_key_name: &'params str,
    wei_change_params: WeiChangeAsHighAndLowBytes,
    params_except_wei_change: Vec<Param<'params>>,
}

#[derive(Debug, PartialEq)]
struct WeiChangeAsHighAndLowBytes {
    high: StdNumParamFormNamed,
    low: StdNumParamFormNamed,
}

#[derive(Debug, PartialEq)]
struct StdNumParamFormNamed {
    name: String,
    value: i64,
}

impl StdNumParamFormNamed {
    fn new(name: String, value: i64) -> Self {
        Self { name, value }
    }
}

pub enum Param<'params> {
    BothClauses(DisplayableRusqliteParamPair<'params>),
    MainClauseLimited(DisplayableRusqliteParamPair<'params>),
}

impl<'params> Param<'params> {
    fn param_pair(&self) -> &DisplayableRusqliteParamPair<'params> {
        match self {
            Param::BothClauses(pair) => pair,
            Param::MainClauseLimited(pair) => pair,
        }
    }

    fn as_rusqlite_param_pair(&'params self) -> RusqliteParamPair<'params> {
        let pair = self.param_pair();
        (pair.0, &pair.1 as &dyn ToSql)
    }
}

impl<'params> From<&'params WeiChangeAsHighAndLowBytes> for [RusqliteParamPair<'params>; 2] {
    fn from(wei_change: &'params WeiChangeAsHighAndLowBytes) -> Self {
        [
            (wei_change.high.name.as_str(), &wei_change.high.value),
            (wei_change.low.name.as_str(), &wei_change.low.value),
        ]
    }
}

impl<'params> SQLParams<'params> {
    fn merge_other_and_wei_params(
        &'params self,
        wei_change_params: [RusqliteParamPair<'params>; 2],
    ) -> Vec<RusqliteParamPair<'params>> {
        Self::merge_params(self.params_except_wei_change.iter(), wei_change_params)
    }

    fn merge_other_and_wei_params_with_conditional_participants(
        &'params self,
        wei_change_params: [RusqliteParamPair<'params>; 2],
    ) -> Vec<RusqliteParamPair<'params>> {
        let other_params_selection = self
            .params_except_wei_change
            .iter()
            .filter(|param| matches!(param, Param::BothClauses(_)));
        Self::merge_params(other_params_selection, wei_change_params)
    }

    fn merge_params(
        params: impl Iterator<Item = &'params Param<'params>>,
        wei_change_params: [RusqliteParamPair<'params>; 2],
    ) -> Vec<RusqliteParamPair<'params>> {
        params
            .map(|param| param.as_rusqlite_param_pair())
            .chain(wei_change_params.into_iter())
            .collect()
    }
}

pub trait TableNameDAO: Debug + Send {
    fn table_name() -> String;
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WeiChange {
    Addition(&'static str, u128),
    //This means that the supplied amount is internally converted into a signed number and
    //so even though the related SQL contains + signs the resulting math operation is subtraction
    Subtraction(&'static str, u128),
}

#[derive(Debug, PartialEq, Eq)]
pub enum BigIntDatabaseError {
    General(String),
    RowChangeMismatch {
        row_key: String,
        detected_count_changed: usize,
    },
}

impl From<BigIntDatabaseError> for PayableDaoError {
    fn from(err: BigIntDatabaseError) -> Self {
        PayableDaoError::RusqliteError(err.to_string())
    }
}

impl From<BigIntDatabaseError> for ReceivableDaoError {
    fn from(err: BigIntDatabaseError) -> Self {
        ReceivableDaoError::RusqliteError(err.to_string())
    }
}

impl Display for BigIntDatabaseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self {
            BigIntDatabaseError::General(msg) => write!(f, "{}", msg),
            BigIntDatabaseError::RowChangeMismatch {
                row_key,
                detected_count_changed,
            } => write!(
                f,
                "Expected 1 row to be changed for the unique key {} but got this count: {}",
                row_key, detected_count_changed
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accountant::db_access_objects::payable_dao::PayableDaoError;
    use crate::accountant::db_big_integer::big_int_db_processor::ByteMagnitude::{High, Low};
    use crate::accountant::db_big_integer::big_int_db_processor::KeyVariants::TestKey;
    use crate::accountant::db_big_integer::big_int_db_processor::WeiChange::{
        Addition, Subtraction,
    };
    use crate::accountant::db_big_integer::test_utils::restricted::{
        create_new_empty_db, test_database_key,
    };
    use crate::database::rusqlite_wrappers::{ConnectionWrapper, ConnectionWrapperReal};
    use crate::test_utils::make_wallet;
    use rusqlite::{Connection, ToSql};
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct DummyDao {}

    impl TableNameDAO for DummyDao {
        fn table_name() -> String {
            String::from("test_table")
        }
    }

    #[test]
    fn display_for_big_int_error_works() {
        assert_eq!(
            BigIntDatabaseError::General("This is a general message".to_string()).to_string(),
            "This is a general message".to_string()
        );
        assert_eq!(
            BigIntDatabaseError::RowChangeMismatch {
                row_key: "Wallet123".to_string(),
                detected_count_changed: 0
            }
            .to_string(),
            "Expected 1 row to be changed for the unique key Wallet123 but got this count: 0"
        )
    }

    #[test]
    fn conversion_from_local_error_to_particular_payable_dao_error_works() {
        assert_eq!(
            PayableDaoError::from(BigIntDatabaseError::General(String::from("booga"))),
            PayableDaoError::RusqliteError("booga".to_string())
        );
        assert_eq!(
            PayableDaoError::from(BigIntDatabaseError::RowChangeMismatch {
                row_key: "booga_key".to_string(),
                detected_count_changed: 2
            }),
            PayableDaoError::RusqliteError(
                "Expected 1 row to be changed for the unique key \
            booga_key but got this count: 2"
                    .to_string()
            )
        );
    }

    #[test]
    fn conversion_from_local_error_to_particular_receivable_dao_error_works() {
        assert_eq!(
            ReceivableDaoError::from(BigIntDatabaseError::General(String::from("blah"))),
            ReceivableDaoError::RusqliteError("blah".to_string())
        );
        assert_eq!(
            ReceivableDaoError::from(BigIntDatabaseError::RowChangeMismatch {
                row_key: "blah_key".to_string(),
                detected_count_changed: 2
            }),
            ReceivableDaoError::RusqliteError(
                "Expected 1 row to be changed for the unique key \
            blah_key but got this count: 2"
                    .to_string()
            )
        );
    }

    #[test]
    fn display_for_byte_magnitude_works() {
        assert_eq!(High.to_string(), "high".to_string());
        assert_eq!(Low.to_string(), "low".to_string())
    }

    #[test]
    fn known_key_variants_to_tuple_of_names_and_val_works() {
        let (wallet_name, wallet_sub_name, wallet_val): (&str, &str, &dyn DisplayableParamValue) =
            KeyVariants::WalletAddress(&"blah").into();
        let (rowid_name, rowid_sub_name, rowid_val): (&str, &str, &dyn DisplayableParamValue) =
            KeyVariants::PendingPayableRowid(&123).into();

        assert_eq!(wallet_name, "wallet_address");
        assert_eq!(wallet_sub_name, ":wallet");
        assert_eq!(wallet_val.to_string(), "blah".to_string());
        assert_eq!(rowid_name, "pending_payable_rowid");
        assert_eq!(rowid_sub_name, ":rowid");
        assert_eq!(rowid_val.to_string(), 123.to_string())
        //cannot be compared by values directly
    }

    #[test]
    fn sql_params_builder_is_nicely_populated_inside_before_calling_build() {
        let subject = SQLParamsBuilder::default();

        let result = subject
            .wei_change(Addition("balance", 4546))
            .key(TestKey {
                var_name: "some_key",
                sub_name: ":some_key",
                val: &"blah",
            })
            .other_params(vec![Param::BothClauses(("other_thing", &46565))]);

        assert_eq!(result.wei_change_spec_opt, Some(Addition("balance", 4546)));
        let key_spec = result.key_spec_opt.unwrap();
        assert_eq!(key_spec.definition_name, "some_key");
        assert_eq!(key_spec.substitution_name_in_sql, ":some_key");
        assert_eq!(key_spec.value_itself.to_string(), "blah".to_string());
        assert!(matches!(
            result.other_params[0].param_pair(),
            ("other_thing", _)
        ));
        assert_eq!(result.other_params.len(), 1)
    }

    #[test]
    fn sql_params_builder_builds_correct_params() {
        let subject = SQLParamsBuilder::default();

        let result = subject
            .wei_change(Addition("balance", 115898))
            .key(TestKey {
                var_name: "some_key",
                sub_name: ":some_key",
                val: &"blah",
            })
            .other_params(vec![Param::BothClauses((":other_thing", &11111))])
            .build();

        assert_eq!(result.table_unique_key_name, "some_key");
        assert_eq!(
            result.wei_change_params,
            WeiChangeAsHighAndLowBytes {
                high: StdNumParamFormNamed::new(":balance_high_b".to_string(), 0),
                low: StdNumParamFormNamed::new(":balance_low_b".to_string(), 115898)
            }
        );
        assert_eq!(
            result.params_except_wei_change[0].param_pair().0,
            ":some_key"
        );
        assert_eq!(
            result.params_except_wei_change[0]
                .param_pair()
                .1
                .to_string(),
            "blah".to_string()
        );
        assert_eq!(
            result.params_except_wei_change[1].param_pair().0,
            ":other_thing"
        );
        assert_eq!(
            result.params_except_wei_change[1]
                .param_pair()
                .1
                .to_string(),
            "11111".to_string()
        );
        assert_eq!(result.params_except_wei_change.len(), 2)
    }

    #[test]
    fn sql_params_builder_builds_correct_params_with_negative_wei_change() {
        let subject = SQLParamsBuilder::default();

        let result = subject
            .wei_change(Subtraction("balance", 454684))
            .key(TestKey {
                var_name: "some_key",
                sub_name: ":some_key",
                val: &"wooow",
            })
            .other_params(vec![Param::BothClauses((":other_thing", &46565))])
            .build();

        assert_eq!(result.table_unique_key_name, "some_key");
        assert_eq!(
            result.wei_change_params,
            WeiChangeAsHighAndLowBytes {
                high: StdNumParamFormNamed::new(":balance_high_b".to_string(), -1),
                low: StdNumParamFormNamed::new(":balance_low_b".to_string(), 9223372036854321124)
            }
        );
        assert_eq!(
            result.params_except_wei_change[0].param_pair().0,
            ":some_key"
        );
        assert_eq!(
            result.params_except_wei_change[0]
                .param_pair()
                .1
                .to_string(),
            "wooow".to_string()
        );
        assert_eq!(
            result.params_except_wei_change[1].param_pair().0,
            ":other_thing"
        );
        assert_eq!(
            result.params_except_wei_change[1]
                .param_pair()
                .1
                .to_string(),
            "46565".to_string()
        );
        assert_eq!(result.params_except_wei_change.len(), 2)
    }

    #[test]
    #[should_panic(expected = "SQLparams cannot miss the component of a key")]
    fn sql_params_builder_cannot_be_built_without_key_spec() {
        let subject = SQLParamsBuilder::default();

        let _ = subject
            .wei_change(Addition("balance", 4546))
            .other_params(vec![Param::BothClauses(("laughter", &"hahaha"))])
            .build();
    }

    #[test]
    #[should_panic(expected = "SQLparams cannot miss the component of wei change")]
    fn sql_params_builder_cannot_be_built_without_wei_change_spec() {
        let subject = SQLParamsBuilder::default();

        let _ = subject
            .key(TestKey {
                var_name: "wallet",
                sub_name: ":wallet",
                val: &make_wallet("wallet"),
            })
            .other_params(vec![Param::BothClauses(("other_thing", &46565))])
            .build();
    }

    #[test]
    fn sql_params_builder_can_be_built_without_other_params_present() {
        let subject = SQLParamsBuilder::default();

        let _ = subject
            .wei_change(Addition("balance", 4546))
            .key(TestKey {
                var_name: "id",
                sub_name: ":id",
                val: &45,
            })
            .build();
    }

    #[test]
    fn merge_other_and_wei_params_with_conditional_participants_can_filter_out_just_update_params()
    {
        let tuple_matrix = [
            ("blah", &456_i64 as &dyn DisplayableParamValue),
            ("super key", &"abcxy"),
            ("time", &779988),
            ("error", &"no threat"),
        ];
        let update_positive_1 = tuple_matrix[0];
        let update_positive_2 = tuple_matrix[1];
        let update_negative_1 = tuple_matrix[2];
        let update_negative_2 = tuple_matrix[3];
        let subject = SQLParams {
            table_unique_key_name: "",
            wei_change_params: WeiChangeAsHighAndLowBytes {
                high: StdNumParamFormNamed {
                    name: "".to_string(),
                    value: 0,
                },
                low: StdNumParamFormNamed {
                    name: "".to_string(),
                    value: 0,
                },
            },
            params_except_wei_change: vec![
                Param::BothClauses(update_positive_2),
                Param::MainClauseLimited(update_negative_1),
                Param::BothClauses(update_positive_1),
                Param::MainClauseLimited(update_negative_2),
            ],
        };

        let result = subject.merge_other_and_wei_params_with_conditional_participants([
            ("always_present_1", &12),
            ("always_present_2", &77),
        ]);

        assert_eq!(result[0].0, update_positive_2.0);
        assert_eq!(result[1].0, update_positive_1.0);
        assert_eq!(result[2].0, "always_present_1");
        assert_eq!(result[3].0, "always_present_2")
    }

    #[test]
    fn return_first_error_works_for_first_error() {
        let results = [Err(Error::GetAuxWrongType), Ok(45465)];

        let err = UpdateOverflowHandlerReal::<DummyDao>::return_first_error(results);

        assert_eq!(err, Err(Error::GetAuxWrongType))
    }

    #[test]
    fn return_first_error_works_for_second_error() {
        let results = [Ok(45465), Err(Error::QueryReturnedNoRows)];

        let err = UpdateOverflowHandlerReal::<DummyDao>::return_first_error(results);

        assert_eq!(err, Err(Error::QueryReturnedNoRows))
    }

    #[test]
    #[should_panic(
        expected = "Broken code: being called to process an error but none was found in [Ok(-45465), Ok(898)]"
    )]
    fn return_first_error_needs_some_error() {
        let results = [Ok(-45465), Ok(898)];

        let err = UpdateOverflowHandlerReal::<DummyDao>::return_first_error(results);

        assert_eq!(err, Err(Error::QueryReturnedNoRows))
    }

    fn make_empty_sql_params<'a>() -> SQLParams<'a> {
        SQLParams {
            table_unique_key_name: "",
            wei_change_params: WeiChangeAsHighAndLowBytes {
                high: StdNumParamFormNamed::new("".to_string(), 0),
                low: StdNumParamFormNamed::new("".to_string(), 0),
            },
            params_except_wei_change: vec![],
        }
    }

    #[test]
    fn determine_command_works_for_upsert() {
        let subject: BigIntSqlConfig<'_, DummyDao> = BigIntSqlConfig {
            main_sql:
                "insert into table (a,b) values ('a','b') on conflict (rowid) do update set etc.",
            overflow_update_clause: "side clause",
            params: make_empty_sql_params(),
            phantom: Default::default(),
        };

        let result = subject.determine_command();

        assert_eq!(result, "upsert".to_string())
    }

    #[test]
    fn determine_command_works_for_update() {
        let subject: BigIntSqlConfig<'_, DummyDao> = BigIntSqlConfig {
            main_sql: "update table set a='a',b='b' where a = 'e'",
            overflow_update_clause: "update with overflow sql",
            params: make_empty_sql_params(),
            phantom: Default::default(),
        };

        let result = subject.determine_command();

        assert_eq!(result, "update".to_string())
    }

    #[test]
    #[should_panic(
        expected = "Sql with simple insert. The processor of big integers is correctly used only if combined with update"
    )]
    fn determine_command_does_not_now_simple_insert() {
        let subject: BigIntSqlConfig<'_, DummyDao> = BigIntSqlConfig {
            main_sql: "insert into table (blah) values ('double blah')",
            overflow_update_clause: "update with overflow sql",
            params: make_empty_sql_params(),
            phantom: Default::default(),
        };

        let _ = subject.determine_command();
    }

    #[test]
    #[should_panic(
        expected = "broken code: unexpected or misplaced command \"some\" in upsert or update, respectively"
    )]
    fn determine_command_panics_if_unknown_command() {
        let subject: BigIntSqlConfig<'_, DummyDao> = BigIntSqlConfig {
            main_sql: "some other sql command",
            overflow_update_clause: "",
            params: make_empty_sql_params(),
            phantom: Default::default(),
        };

        let _ = subject.determine_command();
    }

    #[test]
    fn determine_command_allows_preceding_spaces() {
        let subject: BigIntSqlConfig<'_, DummyDao> = BigIntSqlConfig {
            main_sql: "  update into table (a,b) values ('a','b')",
            overflow_update_clause: "",
            params: make_empty_sql_params(),
            phantom: Default::default(),
        };

        let result = subject.determine_command();

        assert_eq!(result, "update".to_string())
    }

    fn insert_single_record(conn: &dyn ConnectionWrapper, params: [&dyn ToSql; 3]) {
        conn.prepare(
            "insert into test_table (name,balance_high_b, balance_low_b) values (?, ?, ?)",
        )
        .unwrap()
        .execute(params.as_slice())
        .unwrap();
    }

    #[derive(Debug, Default)]
    struct UpdateOverflowHandlerMock {
        update_with_overflow_params: Arc<Mutex<Vec<()>>>,
        update_with_overflow_results: RefCell<Vec<Result<(), BigIntDatabaseError>>>,
    }

    impl<T> UpdateOverflowHandler<T> for UpdateOverflowHandlerMock
    where
        T: TableNameDAO,
    {
        fn update_with_overflow<'a>(
            &self,
            _conn: Either<&dyn ConnectionWrapper, &SecureTransactionWrapper>,
            _config: BigIntSqlConfig<'a, T>,
        ) -> Result<(), BigIntDatabaseError> {
            self.update_with_overflow_params.lock().unwrap().push(());
            self.update_with_overflow_results.borrow_mut().remove(0)
        }
    }

    impl UpdateOverflowHandlerMock {
        fn update_with_overflow_params(mut self, params: &Arc<Mutex<Vec<()>>>) -> Self {
            self.update_with_overflow_params = params.clone();
            self
        }

        fn update_with_overflow_result(self, result: Result<(), BigIntDatabaseError>) -> Self {
            self.update_with_overflow_results.borrow_mut().push(result);
            self
        }
    }

    #[derive(Debug, PartialEq)]
    struct ConventionalUpsertUpdateAnalysisData {
        was_update_with_overflow: bool,
        final_database_values: ReadFinalRow,
    }

    #[derive(Debug, PartialEq)]
    struct ReadFinalRow {
        high_bytes: i64,
        low_bytes: i64,
        as_i128: i128,
    }

    fn analyse_sql_commands_execution_without_details_of_overflow(
        test_name: &str,
        main_sql: &str,
        requested_wei_change: WeiChange,
        init_record: i128,
    ) -> ConventionalUpsertUpdateAnalysisData {
        let update_with_overflow_params_arc = Arc::new(Mutex::new(vec![]));
        let overflow_handler = UpdateOverflowHandlerMock::default()
            .update_with_overflow_params(&update_with_overflow_params_arc)
            .update_with_overflow_result(Ok(()));
        let mut subject = BigIntDbProcessorReal::<DummyDao>::default();
        subject.overflow_handler = Box::new(overflow_handler);

        let act = |conn: &mut dyn ConnectionWrapper| {
            subject.execute(
                Either::Left(conn),
                BigIntSqlConfig::new(
                    main_sql,
                    "",
                    SQLParamsBuilder::default()
                        .key(test_database_key(&"Joe"))
                        .wei_change(requested_wei_change.clone())
                        .build(),
                ),
            )
        };

        precise_upsert_or_update_assertion_test_environment(
            test_name,
            init_record,
            act,
            update_with_overflow_params_arc,
        )
    }

    fn precise_upsert_or_update_assertion_test_environment<F>(
        test_name: &str,
        init_record: i128,
        act: F,
        update_with_overflow_params_arc: Arc<Mutex<Vec<()>>>,
    ) -> ConventionalUpsertUpdateAnalysisData
    where
        F: Fn(&mut dyn ConnectionWrapper) -> Result<(), BigIntDatabaseError>,
    {
        let mut conn = initiate_simple_connection_and_test_table("big_int_db_processor", test_name);
        if init_record != 0 {
            let (init_high, init_low) = BigIntDivider::deconstruct(init_record);
            insert_single_record(conn.as_ref(), [&"Joe", &init_high, &init_low])
        };

        let result = act(conn.as_mut());

        assert_eq!(result, Ok(()));
        let update_with_overflow_params = update_with_overflow_params_arc.lock().unwrap();
        let was_update_with_overflow = !update_with_overflow_params.is_empty();
        assert_on_whole_row(was_update_with_overflow, &*conn, "Joe")
    }

    fn assert_on_whole_row(
        was_update_with_overflow: bool,
        conn: &dyn ConnectionWrapper,
        expected_name: &str,
    ) -> ConventionalUpsertUpdateAnalysisData {
        let final_database_values = conn
            .prepare("select name, balance_high_b, balance_low_b from test_table")
            .unwrap()
            .query_row([], |row| {
                let name = row.get::<usize, String>(0).unwrap();
                assert_eq!(name, expected_name.to_string());
                let high_bytes = row.get::<usize, i64>(1).unwrap();
                let low_bytes = row.get::<usize, i64>(2).unwrap();
                let single_numbered_balance = BigIntDivider::reconstitute(high_bytes, low_bytes);
                Ok(ReadFinalRow {
                    high_bytes,
                    low_bytes,
                    as_i128: single_numbered_balance,
                })
            })
            .unwrap();
        ConventionalUpsertUpdateAnalysisData {
            was_update_with_overflow,
            final_database_values,
        }
    }

    fn create_test_table(conn: &Connection) {
        conn.execute(
            "create table test_table (name text primary key, balance_high_b integer not null, balance_low_b integer not null) strict",
            [],
        )
            .unwrap();
    }

    fn initiate_simple_connection_and_test_table(
        module: &str,
        test_name: &str,
    ) -> Box<ConnectionWrapperReal> {
        let conn = create_new_empty_db(module, test_name);
        create_test_table(&conn);
        Box::new(ConnectionWrapperReal::new(conn))
    }

    const STANDARD_EXAMPLE_OF_UPDATE_CLAUSE: &str = "update test_table set balance_high_b = balance_high_b + :balance_high_b, balance_low_b = balance_low_b + :balance_low_b where name = :name";
    const STANDARD_EXAMPLE_OF_INSERT_CLAUSE: &str = "insert into test_table (name, balance_high_b, balance_low_b) values (:name, :balance_high_b, :balance_low_b)";
    const STANDARD_EXAMPLE_OF_INSERT_WITH_CONFLICT_CLAUSE: &str = "insert into test_table (name, balance_high_b, balance_low_b) values (:name, :balance_high_b, :balance_low_b) on conflict (name) do update set balance_high_b = balance_high_b + :balance_high_b, balance_low_b = balance_low_b + :balance_low_b where name = :name";
    const STANDARD_EXAMPLE_OF_OVERFLOW_UPDATE_CLAUSE: &str = "update test_table set balance_high_b = :balance_high_b, balance_low_b = :balance_low_b where name = :name";

    #[test]
    fn update_alone_works_for_addition() {
        let initial = BigIntDivider::reconstitute(55, 1234567);
        let wei_change = BigIntDivider::reconstitute(1, 22222);

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "update_alone_works_for_addition",
            STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
            Addition("balance", wei_change as u128),
            initial,
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: 56,
                    low_bytes: 1256789,
                    as_i128: initial + wei_change
                }
            }
        )
    }

    #[test]
    fn update_alone_works_for_addition_with_overflow() {
        let initial = BigIntDivider::reconstitute(55, i64::MAX - 5);
        let wei_change = BigIntDivider::reconstitute(1, 6);

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "update_alone_works_for_addition_with_overflow",
            STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
            Addition("balance", wei_change as u128),
            initial,
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: true,
                //overflow halts the update machinery within this specific test, no numeric change
                final_database_values: ReadFinalRow {
                    high_bytes: 55,
                    low_bytes: i64::MAX - 5,
                    as_i128: initial
                }
            }
        )
    }

    #[test]
    fn update_alone_works_for_subtraction() {
        let initial = BigIntDivider::reconstitute(55, i64::MAX - 5);
        let wei_change = -(i64::MAX - 3) as i128;

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "update_alone_works_for_subtraction",
            STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
            Subtraction("balance", wei_change.abs() as u128),
            initial,
        );

        assert_eq!(BigIntDivider::deconstruct(wei_change), (-1, 4));
        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: 54,
                    low_bytes: i64::MAX - 1,
                    as_i128: initial - (-wei_change)
                }
            }
        )
    }

    #[test]
    fn update_alone_works_for_subtraction_with_overflow() {
        let initial = BigIntDivider::reconstitute(55, 4588288282);
        let wei_change: i128 = -12;

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "update_alone_works_for_subtraction_with_overflow",
            STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
            Subtraction("balance", wei_change.abs() as u128),
            initial,
        );

        assert_eq!(
            BigIntDivider::deconstruct(wei_change),
            (-1, 9223372036854775796)
        );
        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: true,
                //overflow halts the update machinery within this specific test, no numeric change
                final_database_values: ReadFinalRow {
                    high_bytes: 55,
                    low_bytes: 4588288282,
                    as_i128: initial
                }
            }
        )
    }

    #[test]
    fn early_return_for_successful_insert_works_for_addition() {
        let initial = BigIntDivider::reconstitute(0, 0);
        let wei_change = BigIntDivider::reconstitute(845, 7788);

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "early_return_for_successful_insert_works",
            STANDARD_EXAMPLE_OF_INSERT_CLAUSE,
            Addition("balance", wei_change as u128),
            initial,
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: 845,
                    low_bytes: 7788,
                    as_i128: wei_change
                }
            }
        )
    }

    #[test]
    fn early_return_for_successful_insert_works_for_subtraction() {
        let initial = BigIntDivider::reconstitute(0, 0);
        let wei_change: i128 = -987654;

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "early_return_for_successful_insert_works_for_subtraction",
            STANDARD_EXAMPLE_OF_INSERT_CLAUSE,
            Subtraction("balance", wei_change.abs() as u128),
            initial,
        );

        assert_eq!(
            BigIntDivider::deconstruct(wei_change),
            (-1, 9223372036853788154)
        );
        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: -1,
                    low_bytes: 9223372036853788154,
                    as_i128: wei_change
                }
            }
        )
    }

    #[test]
    fn insert_blocked_simple_update_succeeds_for_addition() {
        let initial = BigIntDivider::reconstitute(-50, 20);
        let wei_change = BigIntDivider::reconstitute(3, 4);

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "insert_blocked_simple_update_succeeds_for_addition",
            STANDARD_EXAMPLE_OF_INSERT_WITH_CONFLICT_CLAUSE,
            Addition("balance", wei_change as u128),
            initial,
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: -47,
                    low_bytes: 24,
                    as_i128: initial + wei_change
                }
            }
        )
    }

    #[test]
    fn insert_blocked_simple_update_succeeds_for_subtraction() {
        let initial = BigIntDivider::reconstitute(-50, 20);
        let wei_change: i128 = -27670116110564327418;

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "insert_blocked_simple_update_succeeds_for_subtraction",
            STANDARD_EXAMPLE_OF_INSERT_WITH_CONFLICT_CLAUSE,
            Subtraction("balance", wei_change.abs() as u128),
            initial,
        );

        assert_eq!(BigIntDivider::deconstruct(wei_change), (-3, 6));
        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: -53,
                    low_bytes: 26,
                    as_i128: initial - (-wei_change)
                }
            }
        )
    }

    #[test]
    fn insert_blocked_update_with_overflow_for_addition() {
        let initial = BigIntDivider::reconstitute(-50, 20);
        let wei_change = BigIntDivider::reconstitute(8, i64::MAX - 19);

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "insert_blocked_update_with_overflow_for_addition",
            STANDARD_EXAMPLE_OF_INSERT_WITH_CONFLICT_CLAUSE,
            Addition("balance", wei_change as u128),
            initial,
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: true,
                //overflow halts the update machinery within this specific test, no numeric change
                final_database_values: ReadFinalRow {
                    high_bytes: -50,
                    low_bytes: 20,
                    as_i128: initial
                }
            }
        )
    }

    #[test]
    fn insert_blocked_update_with_overflow_for_subtraction() {
        let initial = BigIntDivider::reconstitute(-44, 11);
        let wei_change: i128 = -7;

        let result = analyse_sql_commands_execution_without_details_of_overflow(
            "insert_blocked_update_with_overflow_for_subtraction",
            STANDARD_EXAMPLE_OF_INSERT_WITH_CONFLICT_CLAUSE,
            Subtraction("balance", wei_change.abs() as u128),
            initial,
        );

        assert_eq!(
            BigIntDivider::deconstruct(wei_change),
            (-1, 9223372036854775801)
        );
        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: true,
                //overflow halts the update machinery within this specific test, no numeric change
                final_database_values: ReadFinalRow {
                    high_bytes: -44,
                    low_bytes: 11,
                    as_i128: initial
                }
            }
        );
    }

    #[test]
    fn update_alone_works_also_for_transaction_instead_of_connection() {
        let initial = BigIntDivider::reconstitute(10, 20);
        let wei_change = BigIntDivider::reconstitute(0, 30);
        let subject = BigIntDbProcessorReal::<DummyDao>::default();
        let act = |conn: &mut dyn ConnectionWrapper| {
            let tx = conn.transaction().unwrap();
            let result = subject.execute(
                Either::Right(&tx),
                BigIntSqlConfig::new(
                    STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
                    "",
                    SQLParamsBuilder::default()
                        .key(test_database_key(&"Joe"))
                        .wei_change(Addition("balance", wei_change as u128))
                        .build(),
                ),
            );
            tx.commit().unwrap();
            result
        };
        let result = precise_upsert_or_update_assertion_test_environment(
            "update_alone_works_also_for_transaction_instead_of_connection",
            initial,
            act,
            Arc::new(Mutex::new(vec![])),
        );

        assert_eq!(
            result,
            ConventionalUpsertUpdateAnalysisData {
                was_update_with_overflow: false,
                final_database_values: ReadFinalRow {
                    high_bytes: 10,
                    low_bytes: 50,
                    as_i128: initial + wei_change
                }
            }
        )
    }

    #[test]
    fn main_sql_clause_error_handled() {
        let conn = initiate_simple_connection_and_test_table(
            "big_int_db_processor",
            "main_sql_clause_error_handled",
        );
        let subject = BigIntDbProcessorReal::<DummyDao>::default();
        let balance_change = Addition("balance", 4879898145125);
        let config = BigIntSqlConfig::new(
            "insert into test_table (name, balance_high_b, balance_low_b) values (:name, :balance_wrong_a, :balance_wrong_b) on conflict (name) do \
             update set balance_high_b = balance_high_b + 5, balance_low_b = balance_low_b + 10 where name = :name",
            "",
            SQLParamsBuilder::default()
                .key(test_database_key(&"Joe"))
                .wei_change(balance_change)
                .build(),
        );

        let result = subject.execute(Either::Left(conn.as_ref()), config);

        assert_eq!(
            result,
            Err(BigIntDatabaseError::General(
                "Error from invalid upsert command for test_table table and change of 4879898145125 \
                wei to 'name = Joe' with error 'Invalid parameter name: :balance_high_b'"
                    .to_string()
            ))
        );
    }

    #[test]
    fn different_count_of_changed_rows_than_expected_with_update_only_configuration() {
        let conn = initiate_simple_connection_and_test_table(
            "big_int_db_processor",
            "different_count_of_changed_rows_than_expected_with_update_only_configuration",
        );
        let subject = BigIntDbProcessorReal::<DummyDao>::default();
        let balance_change = Addition("balance", 12345);
        let config = BigIntSqlConfig::new(
            STANDARD_EXAMPLE_OF_UPDATE_CLAUSE,
            "",
            SQLParamsBuilder::default()
                .key(test_database_key(&"Joe"))
                .wei_change(balance_change)
                .build(),
        );

        let result = subject.execute(Either::Left(conn.as_ref()), config);

        assert_eq!(
            result,
            Err(BigIntDatabaseError::RowChangeMismatch {
                row_key: "Joe".to_string(),
                detected_count_changed: 0
            })
        );
    }

    fn update_with_overflow_shared_test_body(
        test_name: &str,
        init_big_initial: i128,
        balance_change: WeiChange,
    ) -> (i64, i64) {
        let conn = initiate_simple_connection_and_test_table("big_int_db_processor", test_name);
        let (init_high_bytes, init_low_bytes) = BigIntDivider::deconstruct(init_big_initial);
        insert_single_record(&*conn, [&"Joe", &init_high_bytes, &init_low_bytes]);
        let update_config = BigIntSqlConfig::new(
            "",
            STANDARD_EXAMPLE_OF_OVERFLOW_UPDATE_CLAUSE,
            SQLParamsBuilder::default()
                .wei_change(balance_change)
                .key(test_database_key(&"Joe"))
                .build(),
        );

        let result = BigIntDbProcessorReal::<DummyDao>::default()
            .overflow_handler
            .update_with_overflow(Either::Left(&*conn), update_config);

        assert_eq!(result, Ok(()));
        let (final_high_bytes, final_low_bytes) = conn
            .prepare("select balance_high_b, balance_low_b from test_table where name = 'Joe'")
            .unwrap()
            .query_row([], |row| {
                let high_bytes = row.get::<usize, i64>(0).unwrap();
                let low_bytes = row.get::<usize, i64>(1).unwrap();
                Ok((high_bytes, low_bytes))
            })
            .unwrap();
        (final_high_bytes, final_low_bytes)
    }

    #[test]
    fn update_with_overflow_for_addition() {
        let big_initial = i64::MAX as i128 * 3;
        let big_addend = i64::MAX as i128 + 454;
        let big_sum = big_initial + big_addend;

        let (final_high_bytes, final_low_bytes) = update_with_overflow_shared_test_body(
            "update_with_overflow_for_addition",
            big_initial,
            Addition("balance", big_addend as u128),
        );

        assert_eq!(
            BigIntDivider::deconstruct(big_initial),
            (2, 9223372036854775805)
        );
        assert_eq!(BigIntDivider::deconstruct(big_addend), (1, 453));
        let result = BigIntDivider::reconstitute(final_high_bytes, final_low_bytes);
        assert_eq!(result, big_sum)
    }

    #[test]
    fn update_with_overflow_for_subtraction_from_positive_num() {
        let big_initial = i64::MAX as i128 * 2;
        let big_subtrahend = i64::MAX as i128 + 120;
        let big_sum = big_initial - big_subtrahend;

        let (final_high_bytes, final_low_bytes) = update_with_overflow_shared_test_body(
            "update_with_overflow_for_subtraction_from_positive_num",
            big_initial,
            Subtraction("balance", big_subtrahend as u128),
        );

        assert_eq!(
            BigIntDivider::deconstruct(big_initial),
            (1, 9223372036854775806)
        );
        assert_eq!(
            BigIntDivider::deconstruct(-big_subtrahend),
            (-2, 9223372036854775689)
        );
        let result = BigIntDivider::reconstitute(final_high_bytes, final_low_bytes);
        assert_eq!(result, big_sum)
    }

    #[test]
    fn update_with_overflow_for_subtraction_from_negative_num() {
        let big_initial = i64::MAX as i128 * 3 + 200;
        let big_subtrahend = i64::MAX as i128 + 120;
        let big_sum = -big_initial - big_subtrahend;

        let (final_high_bytes, final_low_bytes) = update_with_overflow_shared_test_body(
            "update_with_overflow_for_subtraction_from_negative_num",
            -big_initial,
            Subtraction("balance", big_subtrahend as u128),
        );

        assert_eq!(
            BigIntDivider::deconstruct(-big_initial),
            (-4, 9223372036854775611)
        );
        assert_eq!(
            BigIntDivider::deconstruct(-big_subtrahend),
            (-2, 9223372036854775689)
        );
        let result = BigIntDivider::reconstitute(final_high_bytes, final_low_bytes);
        assert_eq!(result, big_sum)
    }

    #[test]
    fn update_with_overflow_handles_unspecific_error() {
        let conn = initiate_simple_connection_and_test_table(
            "big_int_db_processor",
            "update_with_overflow_handles_unspecific_error",
        );
        let balance_change = Addition("balance", 100);
        let update_config = BigIntSqlConfig::new(
            "this can be whatever because the test fails earlier on the select stm",
            STANDARD_EXAMPLE_OF_OVERFLOW_UPDATE_CLAUSE,
            SQLParamsBuilder::default()
                .wei_change(balance_change)
                .key(test_database_key(&"Joe"))
                .build(),
        );

        let result = BigIntDbProcessorReal::<DummyDao>::default()
            .overflow_handler
            .update_with_overflow(Either::Left(conn.as_ref()), update_config);

        //this kind of error is impossible in the real use case but is easiest regarding an arrangement of the test
        assert_eq!(
            result,
            Err(BigIntDatabaseError::General(
                "Updating balance for test_table table and change of 100 wei to 'name = Joe' with \
         error 'Query returned no rows'"
                    .to_string()
            ))
        );
    }

    #[test]
    #[should_panic(
        expected = "Broken code: this code was written to handle one changed row a time, not 2"
    )]
    fn update_with_overflow_is_designed_to_handle_one_record_a_time() {
        let conn = initiate_simple_connection_and_test_table(
            "big_int_db_processor",
            "update_with_overflow_is_designed_to_handle_one_record_a_time",
        );
        insert_single_record(&*conn, [&"Joe", &60, &5555]);
        insert_single_record(&*conn, [&"Jodie", &77, &0]);
        let balance_change = Addition("balance", 100);
        let update_config = BigIntSqlConfig::new(
            "",
            "update test_table set balance_high_b = balance_high_b + :balance_high_b, \
            balance_low_b = balance_low_b + :balance_low_b where name in (:name, 'Jodie')",
            SQLParamsBuilder::default()
                .wei_change(balance_change)
                .key(test_database_key(&"Joe"))
                .build(),
        );

        let _ = BigIntDbProcessorReal::<DummyDao>::default()
            .overflow_handler
            .update_with_overflow(Either::Left(conn.as_ref()), update_config);
    }

    #[test]
    fn update_with_overflow_handles_error_from_executing_the_initial_select_stm() {
        let conn = initiate_simple_connection_and_test_table(
            "big_int_db_processor",
            "update_with_overflow_handles_error_from_executing_the_initial_select_stm",
        );
        conn.prepare("alter table test_table drop column balance_low_b")
            .unwrap()
            .execute([])
            .unwrap();
        conn.prepare("alter table test_table add column balance_low_b text")
            .unwrap()
            .execute([])
            .unwrap();
        insert_single_record(&*conn, [&"Joe", &60, &"bad type"]);
        let balance_change = Addition("balance", 100);
        let update_config = BigIntSqlConfig::new(
            "this can be whatever because the test fails earlier on the select stm",
            "",
            SQLParamsBuilder::default()
                .wei_change(balance_change)
                .key(test_database_key(&"Joe"))
                .build(),
        );

        let result = BigIntDbProcessorReal::<DummyDao>::default()
            .overflow_handler
            .update_with_overflow(Either::Left(conn.as_ref()), update_config);

        assert_eq!(
            result,
            Err(BigIntDatabaseError::General(
                "Updating balance for test_table table and change of 100 wei to 'name = Joe' with error \
        'Invalid column type Text at index: 1, name: balance_low_b'"
                    .to_string()
            ))
        );
    }
}
