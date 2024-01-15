use crate::database::db_migrations::db_migrator::DatabaseMigration;
use crate::database::db_migrations::migrator_utils::DBMigDeclarator;

#[allow(non_camel_case_types)]
pub struct Migrate_9_to_10;

impl DatabaseMigration for Migrate_9_to_10 {
    fn migrate<'a>(
        &self,
        declaration_utils: Box<dyn DBMigDeclarator + 'a>,
    ) -> rusqlite::Result<()> {
        declaration_utils.execute_upon_transaction(&[
            &"INSERT INTO config (name, value, encrypted) VALUES ('start_block', null, 0) ON CONFLICT DO NOTHING",
        ])
    }

    fn old_version(&self) -> usize {
        9
    }
}

#[cfg(test)]
mod tests {
    use crate::database::db_initializer::{
        DbInitializationConfig, DbInitializer, DbInitializerReal, DATABASE_FILE,
    };
    use crate::test_utils::database_utils::{
        bring_db_0_back_to_life_and_return_connection, make_external_data, retrieve_config_row,
    };
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::ensure_node_home_directory_exists;
    use std::fs::create_dir_all;

    #[test]
    fn migration_from_9_to_10_is_properly_set() {
        init_test_logging();
        let dir_path = ensure_node_home_directory_exists(
            "db_migrations",
            "migration_from_9_to_10_is_properly_set",
        );
        create_dir_all(&dir_path).unwrap();
        let db_path = dir_path.join(DATABASE_FILE);
        let _ = bring_db_0_back_to_life_and_return_connection(&db_path);
        let subject = DbInitializerReal::default();

        let result = subject.initialize_to_version(
            &dir_path,
            10,
            DbInitializationConfig::create_or_migrate(make_external_data()),
        );
        let connection = result.unwrap();
        let (mp_value, mp_encrypted) = retrieve_config_row(connection.as_ref(), "start_block");
        let (cs_value, cs_encrypted) = retrieve_config_row(connection.as_ref(), "schema_version");
        assert_eq!(mp_value, None);
        assert_eq!(mp_encrypted, false);
        assert_eq!(cs_value, Some("10".to_string()));
        assert_eq!(cs_encrypted, false);
        TestLogHandler::new().assert_logs_contain_in_order(vec![
            "DbMigrator: Database successfully migrated from version 0 to 1",
            "DbMigrator: Database successfully migrated from version 1 to 2",
            "DbMigrator: Database successfully migrated from version 2 to 3",
            "DbMigrator: Database successfully migrated from version 3 to 4",
            "DbMigrator: Database successfully migrated from version 4 to 5",
            "DbMigrator: Database successfully migrated from version 5 to 6",
            "DbMigrator: Database successfully migrated from version 6 to 7",
            "DbMigrator: Database successfully migrated from version 7 to 8",
            "DbMigrator: Database successfully migrated from version 8 to 9",
            "DbMigrator: Database successfully migrated from version 9 to 10",
        ]);
    }
}