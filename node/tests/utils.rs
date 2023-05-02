// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use masq_lib::blockchains::chains::Chain;
use masq_lib::constants::{CURRENT_LOGFILE_NAME, DEFAULT_CHAIN, DEFAULT_UI_PORT};
use masq_lib::test_utils::utils::{ensure_node_home_directory_exists, node_home_directory};
use masq_lib::utils::{add_chain_specific_directories, localhost};
use node_lib::database::connection_wrapper::ConnectionWrapper;
use node_lib::database::db_initializer::{
    DbInitializationConfig, DbInitializer, DbInitializerReal,
};
use node_lib::test_utils::await_value;
use regex::{Captures, Regex};
use std::env;
use std::io;
use std::net::SocketAddr;
use std::ops::Drop;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[derive(Debug)]
pub struct MASQNode {
    pub data_dir: PathBuf,
    chain: Chain,
    child: Option<process::Child>,
    output: Option<Output>,
}

#[derive(Clone, Debug)]
pub struct CommandConfig {
    pub args: Vec<String>,
}

impl CommandConfig {
    pub fn new() -> CommandConfig {
        CommandConfig { args: vec![] }
    }

    #[allow(dead_code)]
    pub fn opt(mut self, option: &str) -> CommandConfig {
        self.args.push(option.to_string());
        self
    }

    pub fn pair(mut self, option: &str, value: &str) -> CommandConfig {
        self.args.push(option.to_string());
        self.args.push(value.to_string());
        self
    }

    pub fn value_of(&self, parameter: &str) -> Option<String> {
        for n in 0..self.args.len() {
            if self.args[n] == parameter {
                if (n + 1) >= self.args.len() {
                    return None;
                }
                return Some(self.args[n + 1].clone());
            }
        }
        None
    }
}

impl Drop for MASQNode {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

impl MASQNode {
    pub fn path_to_logfile(data_dir: &Path) -> Box<Path> {
        data_dir.join(CURRENT_LOGFILE_NAME).into_boxed_path()
    }

    pub fn path_to_database(data_dir: &PathBuf) -> Box<Path> {
        data_dir.join("node-data.db").into_boxed_path()
    }

    #[allow(dead_code)]
    pub fn output(&mut self) -> Option<Output> {
        self.output.take()
    }

    pub fn start_daemon(
        test_name: &str,
        config_opt: Option<CommandConfig>,
        sterile_database: bool,
        sterile_logfile: bool,
        piped_output: bool,
        ensure_start: bool,
    ) -> MASQNode {
        Self::start_something(
            test_name,
            config_opt,
            sterile_database,
            sterile_logfile,
            piped_output,
            ensure_start,
            Self::make_daemon_command,
        )
    }

    #[allow(dead_code)]
    pub fn start_standard(
        test_name: &str,
        config_opt: Option<CommandConfig>,
        sterile_database: bool,
        sterile_logfile: bool,
        piped_output: bool,
        ensure_start: bool,
    ) -> MASQNode {
        Self::start_something(
            test_name,
            config_opt,
            sterile_database,
            sterile_logfile,
            piped_output,
            ensure_start,
            Self::make_node_command,
        )
    }

    #[allow(dead_code)]
    pub fn start_with_blank_config(
        test_name: &str,
        config_opt: Option<CommandConfig>,
        sterile_database: bool,
        sterile_logfile: bool,
        piped_output: bool,
        ensure_start: bool,
    ) -> MASQNode {
        Self::start_something(
            test_name,
            config_opt,
            sterile_database,
            sterile_logfile,
            piped_output,
            ensure_start,
            Self::make_masqnode_without_initial_config,
        )
    }

    #[allow(dead_code)]
    pub fn run_dump_config(
        test_name: &str,
        config_opt: Option<CommandConfig>,
        sterile_database: bool,
        sterile_logfile: bool,
        piped_output: bool,
        ensure_start: bool,
    ) -> MASQNode {
        Self::start_something(
            test_name,
            config_opt,
            sterile_database,
            sterile_logfile,
            piped_output,
            ensure_start,
            Self::make_dump_config_command,
        )
    }

    #[allow(dead_code)]
    pub fn wait_for_log(&mut self, pattern: &str, limit_ms: Option<u64>) {
        Self::wait_for_match_at_directory(
            pattern,
            &add_chain_specific_directories(self.chain, self.data_dir.as_path()),
            limit_ms,
        );
    }

    pub fn wait_for_match_at_directory(pattern: &str, logfile_dir: &Path, limit_ms: Option<u64>) {
        let logfile_path = Self::path_to_logfile(logfile_dir);
        let do_with_log_output = |log_output: &String, regex: &Regex| -> Option<()> {
            regex.is_match(&log_output[..]).then(|| ())
        };
        Self::wait_for_log_at_directory(
            pattern,
            logfile_path.as_ref(),
            &do_with_log_output,
            limit_ms,
        );
    }

    //gives back all possible captures by given requirements;
    //you can specify how many times the regex needs to be looked for;
    //also allows to define multiple capturing groups and fetch them all at once (the inner vector)
    pub fn capture_pieces_of_log_at_directory(
        pattern: &str,
        logfile_dir: &Path,
        required_number_of_captures: usize,
        limit_ms: Option<u64>,
    ) -> Vec<Vec<String>> {
        let logfile_path = Self::path_to_logfile(logfile_dir);

        let do_with_log_output = |log_output: &String, regex: &Regex| -> Option<Vec<Vec<String>>> {
            let captures = regex
                .captures_iter(&log_output[..])
                .collect::<Vec<Captures>>();
            if captures.len() < required_number_of_captures {
                return None;
            }
            let structured_captures = (0..captures.len())
                .flat_map(|idx| {
                    captures.get(idx).map(|capture| {
                        (0..capture.len())
                            .flat_map(|idx| {
                                capture.get(idx).map(|particular_group_match| {
                                    particular_group_match.as_str().to_string()
                                })
                            })
                            .collect::<Vec<String>>()
                    })
                })
                .collect::<Vec<Vec<String>>>();
            Some(structured_captures)
        };

        Self::wait_for_log_at_directory(
            pattern,
            logfile_path.as_ref(),
            &do_with_log_output,
            limit_ms,
        )
        .unwrap()
    }

    fn wait_for_log_at_directory<T>(
        pattern: &str,
        path_to_logfile: &Path,
        do_with_log_output: &dyn Fn(&String, &Regex) -> Option<T>,
        limit_ms: Option<u64>,
    ) -> Option<T> {
        let regex = regex::Regex::new(pattern).unwrap();
        let real_limit_ms = limit_ms.unwrap_or(0xFFFFFFFF);
        let started_at = Instant::now();
        let mut read_content_opt = None;
        loop {
            match std::fs::read_to_string(&path_to_logfile) {
                Ok(contents) => {
                    read_content_opt = Some(contents.clone());
                    if let Some(result) = do_with_log_output(&contents, &regex) {
                        break Some(result);
                    }
                }
                Err(e) => {
                    eprintln!("Could not read logfile at {:?}: {:?}", path_to_logfile, e);
                }
            };
            assert_eq!(
                MASQNode::millis_since(started_at) < real_limit_ms,
                true,
                "Timeout: waited for more than {}ms without finding '{}' in these logs:\n{}\n",
                real_limit_ms,
                pattern,
                read_content_opt.unwrap_or(String::from("None"))
            );
            thread::sleep(Duration::from_millis(200));
        }
    }

    #[allow(dead_code)]
    pub fn wait_for_exit(&mut self) -> Option<Output> {
        let child_opt = self.child.take();
        let output_opt = self.output.take();
        match (child_opt, output_opt) {
            (None, Some(output)) => {
                self.output = Some(output);
                self.output.clone()
            }
            (Some(child), None) => match child.wait_with_output() {
                Ok(output) => Some(output),
                Err(e) => panic!("{:?}", e),
            },
            (Some(_), Some(_)) => panic!("Internal error: Inconsistent MASQ Node state"),
            (None, None) => None,
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn kill(&mut self) -> Result<process::ExitStatus, io::Error> {
        let child_opt = self.child.take();
        let output_opt = self.output.take();
        Ok(match (child_opt, output_opt) {
            (Some(mut child), None) => {
                child.kill()?;
                let result = child.wait()?;
                self.output = Some(Output {
                    status: result,
                    stdout: vec![],
                    stderr: vec![],
                });
                result
            }
            (None, Some(output)) => {
                let result = output.status.clone();
                self.output = Some(output);
                result
            }
            (Some(_), Some(_)) => panic!("Internal error: Inconsistent MASQ Node state"),
            (None, None) => return Err(io::Error::from(io::ErrorKind::InvalidData)),
        })
    }

    #[cfg(target_os = "windows")]
    pub fn kill(&mut self) -> Result<process::ExitStatus, io::Error> {
        let mut command = process::Command::new("taskkill");
        command.args(&["/IM", "MASQNode.exe", "/F"]);
        let process_output = command
            .output()
            .unwrap_or_else(|e| panic!("Couldn't kill MASQNode.exe: {}", e));
        self.child.take();
        Ok(process_output.status)
    }

    pub fn remove_logfile(data_dir: &PathBuf) -> Box<Path> {
        let logfile_path = Self::path_to_logfile(data_dir);
        match std::fs::remove_file(&logfile_path) {
            Ok(_) => (),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(ref e) => panic!("{:?}", e),
        }
        logfile_path
    }

    pub fn remove_database(data_dir: &PathBuf, chain: Chain) {
        let data_dir_chain_path = add_chain_specific_directories(chain, data_dir);
        let database = Self::path_to_database(&data_dir_chain_path);
        match std::fs::remove_file(&database) {
            Ok(_) => (),
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => panic!(
                "Couldn't remove preexisting database at {:?}: {}",
                database, e
            ),
        }
    }

    fn start_something<F: FnOnce(&PathBuf, Option<CommandConfig>, bool) -> process::Command>(
        test_name: &str,
        config_opt: Option<CommandConfig>,
        sterile_database: bool,
        sterile_logfile: bool,
        piped_streams: bool,
        ensure_start: bool,
        command_getter: F,
    ) -> MASQNode {
        let data_dir = if sterile_database {
            ensure_node_home_directory_exists("integration", test_name)
        } else {
            node_home_directory("integration", test_name)
        };
        let chain = Self::get_chain_from_config(&config_opt);
        if sterile_logfile {
            let _ = Self::remove_logfile(&data_dir);
        }
        let ui_port = Self::ui_port_from_config_opt(&config_opt);
        let mut command = command_getter(&data_dir, config_opt, sterile_database);
        let command = if piped_streams {
            command.stdout(Stdio::piped()).stderr(Stdio::piped())
        } else {
            &mut command
        };
        let mut result = Self::spawn_process(command, data_dir.to_owned(), chain);
        if ensure_start {
            result.wait_for_node(ui_port).unwrap();
        }
        result
    }

    fn spawn_process(cmd: &mut Command, data_dir: PathBuf, chain: Chain) -> MASQNode {
        let child = cmd.spawn().unwrap();
        MASQNode {
            data_dir,
            chain,
            child: Some(child),
            output: None,
        }
    }

    fn get_chain_from_config(config_opt: &Option<CommandConfig>) -> Chain {
        match config_opt {
            Some(config) => match config.value_of("--chain") {
                Some(chain_str) => Chain::from(chain_str.as_str()),
                None => DEFAULT_CHAIN,
            },
            None => DEFAULT_CHAIN,
        }
    }

    fn millis_since(started_at: Instant) -> u64 {
        let interval = Instant::now().duration_since(started_at);
        let second_milliseconds = interval.as_secs() * 1000;
        let nanosecond_milliseconds = (interval.subsec_nanos() as u64) / 1000000;
        second_milliseconds + nanosecond_milliseconds
    }

    fn make_daemon_command(
        data_dir: &PathBuf,
        config_opt: Option<CommandConfig>,
        remove_database: bool,
    ) -> process::Command {
        let chain = Self::get_chain_from_config(&config_opt);
        let mut args = Self::daemon_args();
        args.extend(match config_opt {
            Some(c) => c.args,
            None => vec![],
        });
        Self::start_with_args_extension(chain, data_dir, args, remove_database)
    }

    fn make_node_command(
        data_dir: &PathBuf,
        config_opt: Option<CommandConfig>,
        remove_database: bool,
    ) -> process::Command {
        let chain = Self::get_chain_from_config(&config_opt);
        let mut args = Self::standard_args();
        args.extend(Self::get_extra_args(data_dir, config_opt));
        Self::start_with_args_extension(chain, data_dir, args, remove_database)
    }

    fn make_masqnode_without_initial_config(
        data_dir: &PathBuf,
        config_opt: Option<CommandConfig>,
        remove_database: bool,
    ) -> process::Command {
        let chain = Self::get_chain_from_config(&config_opt);
        let args = Self::get_extra_args(data_dir, config_opt);
        Self::start_with_args_extension(chain, data_dir, args, remove_database)
    }

    fn start_with_args_extension(
        chain: Chain,
        data_dir: &PathBuf,
        additional_args: Vec<String>,
        remove_database: bool,
    ) -> process::Command {
        if remove_database {
            Self::remove_database(data_dir, chain)
        }
        let mut command = command_to_start();
        command.args(additional_args);
        command
    }

    fn make_dump_config_command(
        data_dir: &PathBuf,
        config_opt: Option<CommandConfig>,
        _unused: bool,
    ) -> process::Command {
        let mut command = command_to_start();
        let mut args = Self::dump_config_args();
        args.extend(Self::get_extra_args(data_dir, config_opt));
        command.args(&args);
        command
    }

    fn daemon_args() -> Vec<String> {
        apply_prefix_parameters(CommandConfig::new())
            .opt("--initialization")
            .args
    }

    fn standard_args() -> Vec<String> {
        apply_prefix_parameters(CommandConfig::new())
            .pair("--neighborhood-mode", "zero-hop")
            .pair(
                "--consuming-private-key",
                "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            )
            .pair("--log-level", "trace")
            .args
    }

    #[allow(dead_code)]
    fn dump_config_args() -> Vec<String> {
        apply_prefix_parameters(CommandConfig::new())
            .opt("--dump-config")
            .args
    }

    fn get_extra_args(data_dir: &PathBuf, config_opt: Option<CommandConfig>) -> Vec<String> {
        let mut args = config_opt.unwrap_or(CommandConfig::new()).args;
        if !args.contains(&"--data-directory".to_string()) {
            args.push("--data-directory".to_string());
            args.push(data_dir.to_string_lossy().to_string());
        }
        args
    }

    fn wait_for_node(&mut self, ui_port: u16) -> Result<(), String> {
        let result = await_value(Some((600, 6000)), || {
            let address = SocketAddr::new(localhost(), ui_port);
            match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
                Ok(stream) => {
                    stream.shutdown(std::net::Shutdown::Both).unwrap();
                    Ok(())
                }
                Err(e) => Err(format!("Can't connect yet on port {}: {:?}", ui_port, e)),
            }
        });
        if result.is_err() {
            self.kill().map_err(|e| format!("{:?}", e))?;
        };
        result
    }

    fn ui_port_from_config_opt(config_opt: &Option<CommandConfig>) -> u16 {
        match config_opt {
            None => DEFAULT_UI_PORT,
            Some(config) => match config.value_of("--ui-port") {
                None => DEFAULT_UI_PORT,
                Some(ui_port_string) => ui_port_string.parse::<u16>().unwrap(),
            },
        }
    }
}

#[cfg(target_os = "windows")]
fn command_to_start() -> process::Command {
    process::Command::new("cmd")
}

#[cfg(not(target_os = "windows"))]
fn command_to_start() -> process::Command {
    let test_command = env::args().next().unwrap();
    let debug_or_release = test_command
        .split("/")
        .skip_while(|s| s != &"target")
        .skip(1)
        .next()
        .unwrap();
    let bin_dir = &format!("target/{}", debug_or_release);
    let command_to_start = &format!("{}/MASQNode", bin_dir);
    process::Command::new(command_to_start)
}

#[cfg(target_os = "windows")]
fn apply_prefix_parameters(command_config: CommandConfig) -> CommandConfig {
    command_config.pair("/c", &node_command())
}

#[cfg(not(target_os = "windows"))]
fn apply_prefix_parameters(command_config: CommandConfig) -> CommandConfig {
    command_config
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
fn node_command() -> String {
    let test_command = env::args().next().unwrap();
    let debug_or_release = test_command
        .split("\\")
        .skip_while(|s| s != &"target")
        .skip(1)
        .next()
        .unwrap();
    let bin_dir = &format!("target\\{}", debug_or_release);
    format!("{}\\MASQNode.exe", bin_dir)
}

pub fn make_conn(home_dir: &Path) -> Box<dyn ConnectionWrapper> {
    DbInitializerReal::default()
        .initialize(home_dir, DbInitializationConfig::panic_on_migration())
        .unwrap()
}
