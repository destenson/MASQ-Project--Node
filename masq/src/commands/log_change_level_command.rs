// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::command_context::CommandContext;
use crate::commands::commands_common::{
    send, transaction, Command, CommandError, STANDARD_COMMAND_TIMEOUT_MILLIS,
};
use clap::{App, Arg, SubCommand};
use masq_lib::messages::{UiCrashRequest, UiLogChangeLevelRequest, UiLogChangeLevelResponse};
use std::fmt::Debug;

#[derive(Debug)]
pub struct LogChangeLevelCommand {
    level: String,
}

pub fn log_change_level_subcommand() -> App<'static, 'static> {
    SubCommand::with_name("log-change-level")
        .about("This command will configure the log level for UI Messaging.")
        .arg(
            Arg::with_name("level")
                .help("Messages at this level and above will be sent to UI.")
                .index(1)
                .possible_values(&["Trace", "Debug", "Info", "Warn", "Error"])
                .case_insensitive(true)
                .default_value("Info"),
        )
}

impl Command for LogChangeLevelCommand {
    fn execute(&self, context: &mut dyn CommandContext) -> Result<(), CommandError> {
        let input = UiLogChangeLevelRequest {
            log_level: self.level.clone(),
        };
        let result: Result<UiLogChangeLevelResponse, CommandError> =
            transaction(input, context, STANDARD_COMMAND_TIMEOUT_MILLIS);
        match result {
            Ok(_) => Ok(()),
            Err(e) => todo!("{:?}", e),
        }
    }
}

impl LogChangeLevelCommand {
    pub fn new(pieces: &[String]) -> Result<Self, String> {
        let matches = match log_change_level_subcommand().get_matches_from_safe(pieces) {
            Ok(matches) => matches,
            Err(e) => todo!("{:?}", e),
        };
        Ok(Self {
            level: matches
                .value_of("level")
                .expect("level parameter is not properly defaulted")
                .to_uppercase(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_context::ContextError;
    use crate::command_factory::{CommandFactory, CommandFactoryReal};
    use crate::commands::commands_common::STANDARD_COMMAND_TIMEOUT_MILLIS;
    use crate::commands::crash_command::CrashCommand;
    use crate::test_utils::mocks::CommandContextMock;
    use masq_lib::messages::{ToMessageBody, UiLogChangeLevelRequest, UiLogChangeLevelResponse};
    use std::sync::{Arc, Mutex};

    #[test]
    fn execute_happy_path() {
        let factory = CommandFactoryReal::new();
        let transact_params_arc = Arc::new(Mutex::new(vec![]));
        let mut context = CommandContextMock::new()
            .transact_params(&transact_params_arc)
            .transact_result(Ok(UiLogChangeLevelResponse {}.tmb(0)));
        let subject = factory
            .make(&["log-change-level".to_string(), "Warn".to_string()])
            .unwrap();

        let result = subject.execute(&mut context);

        assert_eq!(result, Ok(()));
        let transact_params = transact_params_arc.lock().unwrap();
        assert_eq!(
            *transact_params,
            vec![(
                UiLogChangeLevelRequest {
                    log_level: "WARN".to_string()
                }
                .tmb(0),
                STANDARD_COMMAND_TIMEOUT_MILLIS
            )]
        )
    }

    #[test]
    fn log_level_command_handles_send_failure() {
        let mut context = CommandContextMock::new()
            .send_result(Err(ContextError::ConnectionDropped("blah".to_string())));
        let subject = CrashCommand::new(&[
            "crash".to_string(),
            "BlockchainBridge".to_string(),
            "message".to_string(),
        ])
        .unwrap();

        let result = subject.execute(&mut context);

        assert_eq!(
            result,
            Err(CommandError::ConnectionProblem("blah".to_string()))
        )
    }
}
