// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::apps::app_head;
use crate::bootstrapper::BootstrapperConfig;
use crate::daemon::dns_inspector::dns_inspector_factory::{
    DnsInspectorFactory, DnsInspectorFactoryReal,
};
use crate::database::db_initializer::{DbInitializer, DbInitializerReal, InitializationError};
use crate::database::db_migrations::MigratorConfig;
use crate::db_config::config_dao_null::ConfigDaoNull;
use crate::db_config::persistent_configuration::{
    PersistentConfiguration, PersistentConfigurationReal,
};
use crate::node_configurator::node_configurator_standard::privileged_parse_args;
use crate::node_configurator::unprivileged_parse_args_configuration::{
    ParseArgsConfiguration, ParseArgsConfigurationDaoNull, ParseArgsConfigurationDaoReal,
};
use crate::node_configurator::{
    data_directory_from_context, determine_config_file_path, DirsWrapper, DirsWrapperReal,
};
use crate::payment_curve_params_computed_default_and_is_required;
use crate::rate_pack_params_computed_default_and_is_required;
use crate::scan_interval_params_computed_default_and_is_required;
use crate::sub_lib::neighborhood::NeighborhoodMode as NeighborhoodModeEnum;
use crate::sub_lib::neighborhood::NodeDescriptor;
use crate::sub_lib::utils::make_new_multi_config;
use crate::test_utils::main_cryptde;
use clap::value_t;
use itertools::Itertools;
use lazy_static::lazy_static;
use masq_lib::blockchains::chains::Chain as BlockChain;
use masq_lib::constants::{
    DEFAULT_CHAIN, DEFAULT_PAYABLE_SCAN_INTERVAL, DEFAULT_PAYMENT_CURVES,
    DEFAULT_PENDING_PAYMENT_SCAN_INTERVAL, DEFAULT_RATE_PACK, DEFAULT_RECEIVABLE_SCAN_INTERVAL,
};
use masq_lib::logger::Logger;
use masq_lib::messages::UiSetupResponseValueStatus::{Blank, Configured, Default, Required, Set};
use masq_lib::messages::{UiSetupRequestValue, UiSetupResponseValue, UiSetupResponseValueStatus};
use masq_lib::multi_config::make_arg_matches_accesible;
use masq_lib::multi_config::{
    CommandLineVcl, ConfigFileVcl, EnvironmentVcl, MultiConfig, VirtualCommandLine,
};
use masq_lib::shared_schema::{shared_app, ConfiguratorError};
use masq_lib::utils::ExpectValue;
use paste::paste;
use std::collections::HashMap;
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

//TODO get this back out
lazy_static! {
    //should stay private
    static ref SETUP_REPORTER_LOGGER: Logger = Logger::new("SetupReporter");
}

const CONSOLE_DIAGNOSTICS: bool = false;

pub type SetupCluster = HashMap<String, UiSetupResponseValue>;

#[cfg(test)]
pub fn setup_cluster_from(input: Vec<(&str, &str, UiSetupResponseValueStatus)>) -> SetupCluster {
    input
        .into_iter()
        .map(|(k, v, s)| (k.to_string(), UiSetupResponseValue::new(k, v, s)))
        .collect::<SetupCluster>()
}

pub trait SetupReporter {
    fn get_modified_setup(
        &self,
        existing_setup: SetupCluster,
        incoming_setup: Vec<UiSetupRequestValue>,
    ) -> Result<SetupCluster, (SetupCluster, ConfiguratorError)>;
}

pub struct SetupReporterReal {
    dirs_wrapper: Box<dyn DirsWrapper>,
    logger: Logger,
}

impl SetupReporter for SetupReporterReal {
    fn get_modified_setup(
        &self,
        mut existing_setup: SetupCluster,
        incoming_setup: Vec<UiSetupRequestValue>,
    ) -> Result<SetupCluster, (SetupCluster, ConfiguratorError)> {
        let default_setup = Self::get_default_params();
        let mut blanked_out_former_values = HashMap::new();
        incoming_setup
            .iter()
            .filter(|v| v.value.is_none())
            .for_each(|v| {
                if let Some(former_value) = existing_setup.remove(&v.name) {
                    blanked_out_former_values.insert(v.name.clone(), former_value);
                };
            });
        //TODO improve this, embarrassing
        //we had troubles at an attempt to blank out this parameter on an error resulting in a diverging chain from the data_dir
        if blanked_out_former_values.get("chain").is_some() {
            let _ = blanked_out_former_values.remove("chain");
        }
        let mut incoming_setup = incoming_setup
            .into_iter()
            .filter(|v| v.value.is_some())
            .map(|v| {
                (
                    v.name.clone(),
                    UiSetupResponseValue::new(&v.name, &v.value.expect("Value disappeared!"), Set),
                )
            })
            .collect::<SetupCluster>();
        let all_but_configured =
            Self::combine_clusters(vec![&default_setup, &existing_setup, &incoming_setup]);
        eprintln_setup("DEFAULTS", &default_setup);
        eprintln_setup("EXISTING", &existing_setup);
        eprintln_setup("BLANKED-OUT FORMER VALUES", &blanked_out_former_values);
        eprintln_setup("INCOMING", &incoming_setup);
        eprintln_setup("ALL BUT CONFIGURED", &all_but_configured);
        let mut error_so_far = ConfiguratorError::new(vec![]);
        let (real_user_opt, data_directory_opt, chain) =
            match Self::calculate_fundamentals(self.dirs_wrapper.as_ref(), &all_but_configured) {
                Ok(triple) => triple,
                Err(error) => {
                    error_so_far.extend(error);
                    (None, None, DEFAULT_CHAIN)
                }
            };
        let real_user = real_user_opt.unwrap_or_else(|| {
            crate::bootstrapper::RealUser::new(None, None, None)
                .populate(self.dirs_wrapper.as_ref())
        });
        let data_directory = match all_but_configured.get("data-directory") {
            Some(uisrv) if uisrv.status == Set => PathBuf::from(&uisrv.value),
            _ => data_directory_from_context(
                self.dirs_wrapper.as_ref(),
                &real_user,
                &data_directory_opt,
                chain,
            ),
        };
        let (configured_setup, error_opt) =
            self.calculate_configured_setup(&all_but_configured, &data_directory);
        if let Some(error) = error_opt {
            error_so_far.extend(error);
        }
        error_so_far.param_errors.iter().for_each(|param_error| {
            let _ = incoming_setup.remove(&param_error.parameter);
        });
        let combined_setup = Self::combine_clusters(vec![&all_but_configured, &configured_setup]);
        eprintln_setup("CONFIGURED", &configured_setup);
        eprintln_setup("COMBINED", &combined_setup);
        let final_setup = value_retrievers(self.dirs_wrapper.as_ref())
            .into_iter()
            .map(|retriever| {
                let make_blank_or_required = || {
                    let status = if retriever.is_required(&combined_setup) {
                        Required
                    } else {
                        Blank
                    };
                    (
                        retriever.value_name().to_string(),
                        UiSetupResponseValue::new(retriever.value_name(), "", status),
                    )
                };
                match combined_setup.get(retriever.value_name()) {
                    Some(uisrv) if vec![Blank, Required].contains(&uisrv.status) => {
                        make_blank_or_required()
                    }
                    Some(uisrv) => (retriever.value_name().to_string(), uisrv.clone()),
                    None => make_blank_or_required(),
                }
            })
            .collect::<SetupCluster>();
        eprintln_setup("FINAL", &final_setup);
        if error_so_far.param_errors.is_empty() {
            Ok(final_setup)
        } else {
            Err((
                Self::combine_clusters(vec![&final_setup, &blanked_out_former_values]),
                error_so_far,
            ))
        }
    }
}

#[allow(dead_code)]
fn eprintln_setup(label: &str, cluster: &SetupCluster) {
    if !CONSOLE_DIAGNOSTICS {
        return;
    }
    let message = cluster
        .iter()
        .map(|(_, v)| (v.name.to_string(), v.value.to_string(), v.status))
        .sorted_by_key(|(n, _, _)| n.clone())
        .map(|(n, v, s)| format!("{:26}{:65}{:?}", n, v, s))
        .join("\n");
    eprintln!("{}:\n{}\n", label, message);
}

impl SetupReporterReal {
    pub fn new(dirs_wrapper: Box<dyn DirsWrapper>) -> Self {
        Self {
            dirs_wrapper,
            logger: Logger::new("SetupReporter"),
        }
    }

    pub fn get_default_params() -> SetupCluster {
        let schema = shared_app(app_head());
        schema
            .p
            .opts
            .iter()
            .flat_map(|opt| {
                let name = opt.b.name;
                match opt.v.default_val {
                    Some(os_str) => {
                        let value = os_str.to_str().expect("expected valid UTF-8");
                        Some((
                            name.to_string(),
                            UiSetupResponseValue::new(name, value, Default),
                        ))
                    }
                    None => None,
                }
            })
            .collect()
    }

    fn real_user_from_str(s: &str) -> Option<crate::bootstrapper::RealUser> {
        match crate::bootstrapper::RealUser::from_str(s) {
            Ok(ru) => Some(ru),
            Err(_) => None,
        }
    }

    fn calculate_fundamentals(
        dirs_wrapper: &dyn DirsWrapper,
        combined_setup: &SetupCluster,
    ) -> Result<
        (
            Option<crate::bootstrapper::RealUser>,
            Option<PathBuf>,
            BlockChain,
        ),
        ConfiguratorError,
    > {
        let multi_config = Self::make_multi_config(dirs_wrapper, None, true, false)?;
        let real_user_opt = match (
            value_m!(multi_config, "real-user", String),
            combined_setup.get("real-user"),
        ) {
            (Some(real_user_str), None) => Self::real_user_from_str(&real_user_str),
            (Some(_), Some(uisrv)) if uisrv.status == Set => Self::real_user_from_str(&uisrv.value),
            (Some(real_user_str), Some(_)) => Self::real_user_from_str(&real_user_str),
            (None, Some(uisrv)) => Self::real_user_from_str(&uisrv.value),
            (None, None) => {
                Some(crate::bootstrapper::RealUser::new(None, None, None).populate(dirs_wrapper))
            }
        };
        let chain_name = match (
            value_m!(multi_config, "chain", String),
            combined_setup.get("chain"),
        ) {
            (Some(chain), None) => chain,
            (Some(_), Some(uisrv)) if uisrv.status == Set => uisrv.value.clone(),
            (Some(chain_str), Some(_)) => chain_str,
            (None, Some(uisrv)) => uisrv.value.clone(),
            (None, None) => DEFAULT_CHAIN.rec().literal_identifier.to_string(),
        };
        let data_directory_opt = match (
            value_m!(multi_config, "data-directory", String),
            combined_setup.get("data-directory"),
        ) {
            (Some(ddir_str), None) => Some(PathBuf::from(&ddir_str)),
            (Some(_), Some(uisrv)) if uisrv.status == Set => Some(PathBuf::from(&uisrv.value)),
            (Some(ddir_str), Some(_)) => Some(PathBuf::from(&ddir_str)),
            _ => None,
        };
        Ok((
            real_user_opt,
            data_directory_opt,
            BlockChain::from(chain_name.as_str()),
        ))
    }

    fn calculate_configured_setup(
        &self,
        combined_setup: &SetupCluster,
        data_directory: &Path,
    ) -> (SetupCluster, Option<ConfiguratorError>) {
        let mut error_so_far = ConfiguratorError::new(vec![]);
        let db_password_opt = combined_setup.get("db-password").map(|v| v.value.clone());
        let command_line = Self::make_command_line(combined_setup);
        let multi_config = match Self::make_multi_config(
            self.dirs_wrapper.as_ref(),
            Some(command_line),
            true,
            true,
        ) {
            Ok(mc) => mc,
            Err(ce) => return (HashMap::new(), Some(ce)),
        };
        let ((bootstrapper_config, persistent_config), error_opt) =
            self.run_configuration(&multi_config, data_directory);
        if let Some(error) = error_opt {
            error_so_far.extend(error);
        }
        let mut setup = value_retrievers(self.dirs_wrapper.as_ref())
            .into_iter()
            .map(|r| {
                let computed_default = r.computed_default_value(
                    &bootstrapper_config,
                    persistent_config.as_ref(),
                    &db_password_opt,
                );
                let configured = match value_m!(multi_config, r.value_name(), String) {
                    Some(value) => UiSetupResponseValue::new(r.value_name(), &value, Configured),
                    None => UiSetupResponseValue::new(r.value_name(), "", Blank),
                };
                let value = Self::choose_uisrv(&computed_default, &configured).clone();
                (r.value_name().to_string(), value)
            })
            .collect::<SetupCluster>();
        match setup.get_mut("config-file") {
            // special case because of early processing
            Some(uisrv) if &uisrv.value == "config.toml" => uisrv.status = Default,
            _ => (),
        };
        if error_so_far.param_errors.is_empty() {
            (setup, None)
        } else {
            (setup, Some(error_so_far))
        }
    }

    fn combine_clusters(clusters: Vec<&SetupCluster>) -> SetupCluster {
        let mut result: SetupCluster = HashMap::new();
        clusters.into_iter().for_each(|cluster| {
            let mut step: SetupCluster = HashMap::new();
            cluster.iter().for_each(|(k, incoming)| {
                match result.get(k) {
                    Some(existing) => {
                        step.insert(k.clone(), Self::choose_uisrv(existing, incoming).clone())
                    }
                    None => step.insert(k.clone(), incoming.clone()),
                };
            });
            result.extend(step);
        });
        result
    }

    fn choose_uisrv<'a>(
        existing: &'a UiSetupResponseValue,
        incoming: &'a UiSetupResponseValue,
    ) -> &'a UiSetupResponseValue {
        if incoming.status.priority() >= existing.status.priority() {
            incoming
        } else {
            existing
        }
    }

    fn make_command_line(setup: &SetupCluster) -> Vec<String> {
        let accepted_statuses = vec![Set, Configured];
        let mut command_line = setup
            .iter()
            .filter(|(_, v)| accepted_statuses.contains(&v.status))
            .flat_map(|(_, v)| vec![format!("--{}", v.name), v.value.clone()])
            .collect::<Vec<String>>();
        command_line.insert(0, "program_name".to_string());
        command_line
    }

    fn make_multi_config<'a>(
        dirs_wrapper: &dyn DirsWrapper,
        command_line_opt: Option<Vec<String>>,
        environment: bool,
        config_file: bool,
    ) -> Result<MultiConfig<'a>, ConfiguratorError> {
        let app = shared_app(app_head());
        let mut vcls: Vec<Box<dyn VirtualCommandLine>> = vec![];
        if let Some(command_line) = command_line_opt.clone() {
            vcls.push(Box::new(CommandLineVcl::new(command_line)));
        }
        if environment {
            vcls.push(Box::new(EnvironmentVcl::new(&app)));
        }
        if config_file {
            let command_line = match command_line_opt {
                Some(command_line) => command_line,
                None => vec![],
            };
            let (config_file_path, user_specified) =
                determine_config_file_path(dirs_wrapper, &app, &command_line)?;
            let config_file_vcl = match ConfigFileVcl::new(&config_file_path, user_specified) {
                Ok(cfv) => cfv,
                Err(e) => return Err(ConfiguratorError::required("config-file", &e.to_string())),
            };
            vcls.push(Box::new(config_file_vcl));
        }
        make_new_multi_config(&app, vcls)
    }

    #[allow(clippy::type_complexity)]
    fn run_configuration(
        &self,
        multi_config: &MultiConfig,
        data_directory: &Path,
    ) -> (
        (BootstrapperConfig, Box<dyn PersistentConfiguration>),
        Option<ConfiguratorError>,
    ) {
        let mut error_so_far = ConfiguratorError::new(vec![]);
        let mut bootstrapper_config = BootstrapperConfig::new();
        bootstrapper_config.data_directory = data_directory.to_path_buf();
        match privileged_parse_args(
            self.dirs_wrapper.as_ref(),
            multi_config,
            &mut bootstrapper_config,
        ) {
            Ok(_) => (),
            Err(ce) => {
                error_so_far.extend(ce);
            }
        };
        let initializer = DbInitializerReal::default();
        match initializer.initialize(
            data_directory,
            false,
            MigratorConfig::migration_suppressed_with_error(),
        ) {
            Ok(conn) => {
                let pars_args_configuration = ParseArgsConfigurationDaoReal {};
                let mut persistent_config = PersistentConfigurationReal::from(conn);
                match pars_args_configuration.unprivileged_parse_args(
                    multi_config,
                    &mut bootstrapper_config,
                    &mut persistent_config,
                    &self.logger,
                ) {
                    Ok(_) => ((bootstrapper_config, Box::new(persistent_config)), None),
                    Err(ce) => {
                        error_so_far.extend(ce);
                        (
                            (bootstrapper_config, Box::new(persistent_config)),
                            Some(error_so_far),
                        )
                    }
                }
            }
            Err(InitializationError::Nonexistent | InitializationError::SuppressedMigration) => {
                // When the Daemon runs for the first time, the database will not yet have been
                // created. If the database is old, it should not be used by the Daemon.
                let pars_args_configuration = ParseArgsConfigurationDaoNull {};
                let mut persistent_config =
                    PersistentConfigurationReal::new(Box::new(ConfigDaoNull::default()));
                match pars_args_configuration.unprivileged_parse_args(
                    multi_config,
                    &mut bootstrapper_config,
                    &mut persistent_config,
                    &self.logger,
                ) {
                    Ok(_) => ((bootstrapper_config, Box::new(persistent_config)), None),
                    Err(ce) => {
                        error_so_far.extend(ce);

                        (
                            (bootstrapper_config, Box::new(persistent_config)),
                            Some(error_so_far),
                        )
                    }
                }
            }
            Err(e) => panic!("Couldn't initialize database: {:?}", e),
        }
    }
}

trait ValueRetriever {
    fn value_name(&self) -> &'static str;

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config_opt: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        None
    }

    fn computed_default_value(
        &self,
        bootstrapper_config: &BootstrapperConfig,
        persistent_config: &dyn PersistentConfiguration,
        db_password_opt: &Option<String>,
    ) -> UiSetupResponseValue {
        match self.computed_default(bootstrapper_config, persistent_config, db_password_opt) {
            Some((value, status)) => UiSetupResponseValue::new(self.value_name(), &value, status),
            None => UiSetupResponseValue::new(self.value_name(), "", Blank),
        }
    }

    fn set_value(&self, multi_config: &MultiConfig) -> Option<String> {
        value_m!(multi_config, self.value_name(), String)
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        false
    }
}

fn is_required_for_blockchain(params: &SetupCluster) -> bool {
    !matches! (params.get("neighborhood-mode"), Some(nhm) if &nhm.value == "zero-hop")
}

struct BlockchainServiceUrl {}
impl ValueRetriever for BlockchainServiceUrl {
    fn value_name(&self) -> &'static str {
        "blockchain-service-url"
    }

    fn is_required(&self, params: &SetupCluster) -> bool {
        is_required_for_blockchain(params)
    }
}

struct Chain {}
impl ValueRetriever for Chain {
    fn value_name(&self) -> &'static str {
        "chain"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        Some((DEFAULT_CHAIN.rec().literal_identifier.to_string(), Default))
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        true
    }
}

struct ClandestinePort {}
impl ValueRetriever for ClandestinePort {
    fn value_name(&self) -> &'static str {
        "clandestine-port"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        match persistent_config.clandestine_port() {
            Ok(clandestine_port) => Some((clandestine_port.to_string(), Configured)),
            Err(_) => None,
        }
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        true
    }
}

struct ConfigFile {}
impl ValueRetriever for ConfigFile {
    fn value_name(&self) -> &'static str {
        "config-file"
    }
}

struct ConsumingPrivateKey {}
impl ValueRetriever for ConsumingPrivateKey {
    fn value_name(&self) -> &'static str {
        "consuming-private-key"
    }
}

struct CrashPoint {}
impl ValueRetriever for CrashPoint {
    fn value_name(&self) -> &'static str {
        "crash-point"
    }
}

struct DataDirectory {
    dirs_wrapper: Box<dyn DirsWrapper>,
}
impl ValueRetriever for DataDirectory {
    fn value_name(&self) -> &'static str {
        "data-directory"
    }

    fn computed_default(
        &self,
        bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        let real_user = &bootstrapper_config.real_user;
        let chain = bootstrapper_config.blockchain_bridge_config.chain;
        let data_directory_opt = None;
        Some((
            data_directory_from_context(
                self.dirs_wrapper.as_ref(),
                real_user,
                &data_directory_opt,
                chain,
            )
            .to_string_lossy()
            .to_string(),
            Default,
        ))
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        true
    }
}
impl std::default::Default for DataDirectory {
    fn default() -> Self {
        Self::new(&DirsWrapperReal)
    }
}
impl DataDirectory {
    pub fn new(dirs_wrapper: &dyn DirsWrapper) -> Self {
        Self {
            dirs_wrapper: dirs_wrapper.dup(),
        }
    }
}

struct DbPassword {}
impl ValueRetriever for DbPassword {
    fn value_name(&self) -> &'static str {
        "db-password"
    }

    fn is_required(&self, params: &SetupCluster) -> bool {
        is_required_for_blockchain(params)
    }
}

struct DnsServers {
    factory: Box<dyn DnsInspectorFactory>,
}
impl DnsServers {
    pub fn new() -> Self {
        Self {
            factory: Box::new(DnsInspectorFactoryReal::new()),
        }
    }
}
impl ValueRetriever for DnsServers {
    fn value_name(&self) -> &'static str {
        "dns-servers"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        let inspector = self.factory.make()?;
        match inspector.inspect() {
            Ok(ip_addrs) => {
                if ip_addrs.is_empty() {
                    return None;
                }
                if ip_addrs.iter().any(|ip_addr| ip_addr.is_loopback()) {
                    return None;
                }
                let dns_servers = ip_addrs
                    .into_iter()
                    .map(|ip_addr| ip_addr.to_string())
                    .join(",");
                Some((dns_servers, Default))
            }
            Err(e) => {
                warning!(
                    SETUP_REPORTER_LOGGER,
                    "Error inspecting DNS settings: {:?}",
                    e
                );
                None
            }
        }
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        !matches!(_params.get("neighborhood-mode"), Some(nhm) if &nhm.value == "consume-only")
    }
}

struct EarningWallet {}
impl ValueRetriever for EarningWallet {
    fn value_name(&self) -> &'static str {
        "earning-wallet"
    }
}

struct GasPrice {}
impl ValueRetriever for GasPrice {
    fn value_name(&self) -> &'static str {
        "gas-price"
    }

    fn computed_default(
        &self,
        bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        Some((
            bootstrapper_config
                .blockchain_bridge_config
                .gas_price
                .to_string(),
            Default,
        ))
    }

    fn is_required(&self, params: &SetupCluster) -> bool {
        is_required_for_blockchain(params)
    }
}

struct Ip {}
impl ValueRetriever for Ip {
    fn value_name(&self) -> &'static str {
        "ip"
    }

    fn computed_default(
        &self,
        bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        let neighborhood_mode = &bootstrapper_config.neighborhood_config.mode;
        match neighborhood_mode {
            NeighborhoodModeEnum::Standard(node_addr, _, _)
                if node_addr.ip_addr() == IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)) =>
            {
                Some(("".to_string(), UiSetupResponseValueStatus::Blank))
            }
            NeighborhoodModeEnum::Standard(node_addr, _, _) => Some((
                node_addr.ip_addr().to_string(),
                UiSetupResponseValueStatus::Set,
            )),
            _ => Some(("".to_string(), UiSetupResponseValueStatus::Blank)),
        }
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        false
    }
}

struct LogLevel {}
impl ValueRetriever for LogLevel {
    fn value_name(&self) -> &'static str {
        "log-level"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        Some(("warn".to_string(), Default))
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        true
    }
}

struct MappingProtocol {}
impl ValueRetriever for MappingProtocol {
    fn value_name(&self) -> &'static str {
        "mapping-protocol"
    }

    fn computed_default(
        &self,
        bootstrapper_config: &BootstrapperConfig,
        persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        let persistent_mapping_protocol = match persistent_config.mapping_protocol() {
            Ok(protocol_opt) => protocol_opt,
            Err(_) => None,
        };
        let from_bootstrapper_opt = bootstrapper_config.mapping_protocol_opt;
        match (persistent_mapping_protocol, from_bootstrapper_opt) {
            (Some(persistent), None) => Some((persistent.to_string().to_lowercase(), Configured)),
            (None, Some(from_bootstrapper)) => {
                Some((from_bootstrapper.to_string().to_lowercase(), Configured))
            }
            (Some(persistent), Some(from_bootstrapper)) if persistent != from_bootstrapper => {
                Some((from_bootstrapper.to_string().to_lowercase(), Configured))
            }
            (Some(persistent), Some(_)) => {
                Some((persistent.to_string().to_lowercase(), Configured))
            }
            _ => None,
        }
    }
}

struct NeighborhoodMode {}
impl ValueRetriever for NeighborhoodMode {
    fn value_name(&self) -> &'static str {
        "neighborhood-mode"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        Some(("standard".to_string(), Default))
    }

    fn is_required(&self, _params: &SetupCluster) -> bool {
        true
    }
}

fn node_descriptors_to_neighbors(node_descriptors: Vec<NodeDescriptor>) -> String {
    node_descriptors
        .into_iter()
        .map(|nd| nd.to_string(main_cryptde()))
        .collect_vec()
        .join(",")
}

struct Neighbors {}
impl ValueRetriever for Neighbors {
    fn value_name(&self) -> &'static str {
        "neighbors"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        persistent_config: &dyn PersistentConfiguration,
        db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        match db_password_opt {
            Some(pw) => match persistent_config.past_neighbors(pw) {
                Ok(Some(pns)) => Some((node_descriptors_to_neighbors(pns), Configured)),
                _ => None,
            },
            None => None,
        }
    }

    fn is_required(&self, params: &SetupCluster) -> bool {
        match params.get("neighborhood-mode") {
            Some(nhm) if &nhm.value == "standard" => false,
            Some(nhm) if &nhm.value == "zero-hop" => false,
            _ => true,
        }
    }
}

struct BalanceDecreasesForSec {}
impl ValueRetriever for BalanceDecreasesForSec {
    fn value_name(&self) -> &'static str {
        "balance-decreases-for"
    }

    payment_curve_params_computed_default_and_is_required!("balance_decreases_for_sec");
}

struct BalanceToDecreaseFromGwei {}
impl ValueRetriever for BalanceToDecreaseFromGwei {
    fn value_name(&self) -> &'static str {
        "balance-to-decrease-from"
    }

    payment_curve_params_computed_default_and_is_required!("balance_to_decrease_from_gwei");
}

struct ExitByteRate {}
impl ValueRetriever for ExitByteRate {
    fn value_name(&self) -> &'static str {
        "exit-byte-rate"
    }

    rate_pack_params_computed_default_and_is_required!("exit_byte_rate");
}
struct ExitServiceRate {}
impl ValueRetriever for ExitServiceRate {
    fn value_name(&self) -> &'static str {
        "exit-service-rate"
    }

    rate_pack_params_computed_default_and_is_required!("exit_service_rate");
}
struct PayableScanInterval {}
impl ValueRetriever for PayableScanInterval {
    fn value_name(&self) -> &'static str {
        "payable-scan-interval"
    }

    scan_interval_params_computed_default_and_is_required!(
        "payable_scan_interval",
        DEFAULT_PAYABLE_SCAN_INTERVAL
    );
}
struct PaymentSuggestedAfterSec {}
impl ValueRetriever for PaymentSuggestedAfterSec {
    fn value_name(&self) -> &'static str {
        "payment-suggested-after"
    }

    payment_curve_params_computed_default_and_is_required!("payment_suggested_after_sec");
}
struct PaymentGraceBeforeBanSec {}
impl ValueRetriever for PaymentGraceBeforeBanSec {
    fn value_name(&self) -> &'static str {
        "payment-grace-before-ban"
    }

    payment_curve_params_computed_default_and_is_required!("payment_grace_before_ban_sec");
}
struct PendingPaymentScanInterval {}
impl ValueRetriever for PendingPaymentScanInterval {
    fn value_name(&self) -> &'static str {
        "pending-payment-scan-interval"
    }

    scan_interval_params_computed_default_and_is_required!(
        "pending_payment_scan_interval",
        DEFAULT_PENDING_PAYMENT_SCAN_INTERVAL
    );
}
struct PermanentDebtAllowedGwei {}
impl ValueRetriever for PermanentDebtAllowedGwei {
    fn value_name(&self) -> &'static str {
        "permanent-debt-allowed"
    }

    payment_curve_params_computed_default_and_is_required!("permanent_debt_allowed_gwei");
}

struct ReceivableScanInterval {}
impl ValueRetriever for ReceivableScanInterval {
    fn value_name(&self) -> &'static str {
        "receivable-scan-interval"
    }

    scan_interval_params_computed_default_and_is_required!(
        "receivable_scan_interval",
        DEFAULT_RECEIVABLE_SCAN_INTERVAL
    );
}

struct RoutingByteRate {}
impl ValueRetriever for RoutingByteRate {
    fn value_name(&self) -> &'static str {
        "routing-byte-rate"
    }

    rate_pack_params_computed_default_and_is_required!("routing_byte_rate");
}

struct RoutingServiceRate {}
impl ValueRetriever for RoutingServiceRate {
    fn value_name(&self) -> &'static str {
        "routing-service-rate"
    }

    rate_pack_params_computed_default_and_is_required!("routing_service_rate");
}

struct UnbanWhenBalanceBelowGwei {}
impl ValueRetriever for UnbanWhenBalanceBelowGwei {
    fn value_name(&self) -> &'static str {
        "unban-when-balance-below"
    }
    payment_curve_params_computed_default_and_is_required!("unban_when_balance_below_gwei");
}

//this allows me to avoid excessive test-set inflation,
//just proving this logic is used for every parameter of this kind
trait PaymentCurvesComputedDefaultEvaluation{
    fn computed_default_payment_curves(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64,
    ) -> Option<(String, UiSetupResponseValueStatus)>;
}

struct PaymentCurvesComputedDefaultEvaluationReal{}

impl PaymentCurvesComputedDefaultEvaluation for PaymentCurvesComputedDefaultEvaluationReal{
    fn computed_default_payment_curves(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        todo!()
    }
}

//this allows me to avoid excessive test-set inflation,
//just proving this logic is used for every parameter of this kind
trait ScanIntervalsComputedDefaultEvaluation{
    fn computed_default_scan_intervals(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64,
    ) -> Option<(String, UiSetupResponseValueStatus)>;
}

struct ScanIntervalsComputedDefaultEvaluationReal{}

impl ScanIntervalsComputedDefaultEvaluation for ScanIntervalsComputedDefaultEvaluationReal{
    fn computed_default_scan_intervals(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        todo!()
    }
}

//this allows me to avoid excessive test-set inflation,
//just proving this logic is used for every parameter of this kind
trait RatePackComputedDefaultEvaluation{
    fn computed_default_rate_pack(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64,
    ) -> Option<(String, UiSetupResponseValueStatus)>;
}

struct RatePackComputedDefaultEvaluationReal{}

impl RatePackComputedDefaultEvaluation for RatePackComputedDefaultEvaluationReal{
    fn computed_default_rate_pack(
        bootstrapper_config_value_opt: &Option<u64>,
        persistent_config_value: u64,
        default: u64
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        todo!()
    }
}

//TODO should finally flow into the trait's method and evaporate
fn computed_default_rate_pack(
    bootstrapper_config_value_opt: &Option<u64>,
    persistent_config_value: u64,
    default: u64,
) -> Option<(String, UiSetupResponseValueStatus)>
{
    match bootstrapper_config_value_opt{
        Some(rate) =>
            if *rate == default {
                Some((default.to_string(), Default))
            } else if *rate == persistent_config_value {
                Some((persistent_config_value.to_string(), Configured))
            } else {
                None
            }
        None => None
    }
}

//TODO if the bootstrapper config value is believed to be ever Some() we don't need the Option
fn computed_default_payment_curves_and_scan_intervals_inner_body<T>(
    bootstrapper_config_value_opt: &Option<T>,
    persistent_config_value: T,
    default: T,
) -> Option<(String, UiSetupResponseValueStatus)>
    where
        T: PartialEq + Display + Copy,
{
    let value =
        bootstrapper_config_value_opt.expect("bootstrapper config should've been populated now");
    if value == default {
        Some((default.to_string(), Default))
    } else if value == persistent_config_value {
        Some((persistent_config_value.to_string(), Configured))
    } else {
        None
    }
}

#[macro_export]
macro_rules! payment_curve_params_computed_default_and_is_required {
    ($field_name: literal) => {
        paste! {
                fn computed_default(
                &self,
                bootstrapper_config: &BootstrapperConfig,
                pc: &dyn PersistentConfiguration,
                _db_password_opt: &Option<String>,
            ) -> Option<(String, UiSetupResponseValueStatus)> {
                let bootstrapper_value_opt = bootstrapper_config
                    .accountant_config_opt
                    .as_ref()
                    .map(|accountant_config| accountant_config
                        .payment_curves.[<$field_name>]
                    );
                let pc_value = pc
                        .[<$field_name>]()
                        .expectv($field_name) as i64;
                computed_default_payment_curves_and_scan_intervals_inner_body(
                    &bootstrapper_value_opt,
                    pc_value,
                    DEFAULT_PAYMENT_CURVES.[<$field_name>],
                )
            }

               fn is_required(&self, _params: &SetupCluster) -> bool {true}
        }
    };
}

#[macro_export]
macro_rules! scan_interval_params_computed_default_and_is_required {
    ($field_name: literal,$default: expr) => {
        paste! {
                fn computed_default(
                &self,
                bootstrapper_config: &BootstrapperConfig,
                pc: &dyn PersistentConfiguration,
                _db_password_opt: &Option<String>,
            ) -> Option<(String, UiSetupResponseValueStatus)> {
                let bootstrapper_value_opt = bootstrapper_config
                    .accountant_config_opt
                    .as_ref()
                    .map(|accountant_config| accountant_config
                        .[<$field_name>].as_secs()
                    );
                let pc_value = pc
                        .[<$field_name>]()
                        .expectv($field_name);
                computed_default_payment_curves_and_scan_intervals_inner_body(
                    &bootstrapper_value_opt,
                    pc_value,
                    $default,
                )
            }

                fn is_required(&self, _params: &SetupCluster) -> bool {true}
        }
    };
}

#[macro_export]
macro_rules! rate_pack_params_computed_default_and_is_required {
    ($field_name: literal) => {
        paste! {
                fn computed_default(
                &self,
                bootstrapper_config: &BootstrapperConfig,
                pc: &dyn PersistentConfiguration,
                _db_password_opt: &Option<String>,
            ) -> Option<(String, UiSetupResponseValueStatus)> {
                let neighborhood_mode = &bootstrapper_config.neighborhood_config.mode;
                let bootstrapper_value_opt =
                    if let NeighborhoodModeEnum::Standard(_,_,rate_pack) = neighborhood_mode
                        {Some(rate_pack.[<$field_name>])}
                    else if let NeighborhoodModeEnum::OriginateOnly(_,rate_pack) = neighborhood_mode
                        {Some(rate_pack.[<$field_name>])}
                    else {None};
                let pc_value = pc
                        .[<$field_name>]()
                        .expectv($field_name);
                computed_default_rate_pack(
                    &bootstrapper_value_opt,
                    pc_value,
                    DEFAULT_RATE_PACK.[<$field_name>],
                )
            }

                fn is_required(&self, params: &SetupCluster) -> bool {
                       match params.get("neighborhood-mode") {
                        Some(nhm) if &nhm.value == "standard" => true,
                        Some(nhm) if &nhm.value == "originate-only" => true,
                         _ => false,
                        }
                }
        }
    };
}

struct RealUser {
    #[allow(dead_code)]
    dirs_wrapper: Box<dyn DirsWrapper>,
}
impl ValueRetriever for RealUser {
    fn value_name(&self) -> &'static str {
        "real-user"
    }

    fn computed_default(
        &self,
        _bootstrapper_config: &BootstrapperConfig,
        _persistent_config: &dyn PersistentConfiguration,
        _db_password_opt: &Option<String>,
    ) -> Option<(String, UiSetupResponseValueStatus)> {
        #[cfg(target_os = "windows")]
        {
            None
        }
        #[cfg(not(target_os = "windows"))]
        {
            Some((
                crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(self.dirs_wrapper.as_ref())
                    .to_string(),
                Default,
            ))
        }
    }
}
impl std::default::Default for RealUser {
    fn default() -> Self {
        Self::new(&DirsWrapperReal {})
    }
}
impl RealUser {
    pub fn new(dirs_wrapper: &dyn DirsWrapper) -> Self {
        Self {
            dirs_wrapper: dirs_wrapper.dup(),
        }
    }
}

fn value_retrievers(dirs_wrapper: &dyn DirsWrapper) -> Vec<Box<dyn ValueRetriever>> {
    vec![
        Box::new(BlockchainServiceUrl {}),
        Box::new(Chain {}),
        Box::new(ClandestinePort {}),
        Box::new(ConfigFile {}),
        Box::new(ConsumingPrivateKey {}),
        Box::new(CrashPoint {}),
        Box::new(DataDirectory::new(dirs_wrapper)),
        Box::new(DbPassword {}),
        Box::new(DnsServers::new()),
        Box::new(EarningWallet {}),
        Box::new(GasPrice {}),
        Box::new(Ip {}),
        Box::new(LogLevel {}),
        Box::new(MappingProtocol {}),
        Box::new(NeighborhoodMode {}),
        Box::new(Neighbors {}),
        Box::new(BalanceDecreasesForSec {}),
        Box::new(BalanceToDecreaseFromGwei {}),
        Box::new(ExitByteRate {}),
        Box::new(ExitServiceRate {}),
        Box::new(PayableScanInterval {}),
        Box::new(PaymentSuggestedAfterSec {}),
        Box::new(PaymentGraceBeforeBanSec {}),
        Box::new(PendingPaymentScanInterval {}),
        Box::new(PermanentDebtAllowedGwei {}),
        Box::new(ReceivableScanInterval {}),
        #[cfg(not(target_os = "windows"))]
        Box::new(RealUser::new(dirs_wrapper)),
        Box::new(RoutingByteRate {}),
        Box::new(RoutingServiceRate {}),
        Box::new(UnbanWhenBalanceBelowGwei {}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrapper::RealUser;
    use crate::daemon::dns_inspector::dns_inspector::DnsInspector;
    use crate::daemon::dns_inspector::DnsInspectionError;
    use crate::database::connection_wrapper::ConnectionWrapperReal;
    use crate::database::db_initializer::{DbInitializer, DbInitializerReal, DATABASE_FILE};
    use crate::db_config::config_dao::{ConfigDaoRead, ConfigDaoReal};
    use crate::db_config::persistent_configuration::{
        PersistentConfigError, PersistentConfiguration, PersistentConfigurationReal,
    };
    use crate::node_configurator::{DirsWrapper, DirsWrapperReal};
    use crate::node_test_utils::DirsWrapperMock;
    use crate::sub_lib::cryptde::PublicKey;
    use crate::sub_lib::node_addr::NodeAddr;
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::{assert_string_contains, rate_pack};
    use crate::test_utils::database_utils::bring_db_of_version_0_back_to_life_and_return_connection;
    use crate::test_utils::persistent_configuration_mock::PersistentConfigurationMock;
    use crate::test_utils::unshared_test_utils::make_populated_accountant_config_with_defaults;
    use crate::test_utils::unshared_test_utils::{
        make_persistent_config_real_with_config_dao_null,
        make_pre_populated_mocked_directory_wrapper, make_simplified_multi_config,
    };
    use core::time::Duration;
    use masq_lib::blockchains::chains::Chain as Blockchain;
    use masq_lib::constants::{
        DEFAULT_CHAIN, DEFAULT_PAYABLE_SCAN_INTERVAL, DEFAULT_PAYMENT_CURVES,
        DEFAULT_PENDING_PAYMENT_SCAN_INTERVAL, DEFAULT_RATE_PACK, DEFAULT_RECEIVABLE_SCAN_INTERVAL,
    };
    use masq_lib::messages::UiSetupResponseValueStatus::{Blank, Configured, Required, Set};
    use masq_lib::test_utils::environment_guard::{ClapGuard, EnvironmentGuard};
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::{ensure_node_home_directory_exists, TEST_DEFAULT_CHAIN};
    use masq_lib::utils::localhost;
    use masq_lib::utils::AutomapProtocol;
    use std::cell::RefCell;
    use std::convert::TryFrom;
    #[cfg(not(target_os = "windows"))]
    use std::default::Default;
    use std::fs::File;
    use std::io::Write;
    use std::net::IpAddr;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    pub struct DnsInspectorMock {
        inspect_results: RefCell<Vec<Result<Vec<IpAddr>, DnsInspectionError>>>,
    }

    impl DnsInspector for DnsInspectorMock {
        fn inspect(&self) -> Result<Vec<IpAddr>, DnsInspectionError> {
            self.inspect_results.borrow_mut().remove(0)
        }
    }

    impl DnsInspectorMock {
        pub fn new() -> DnsInspectorMock {
            DnsInspectorMock {
                inspect_results: RefCell::new(vec![]),
            }
        }

        pub fn inspect_result(
            self,
            result: Result<Vec<IpAddr>, DnsInspectionError>,
        ) -> DnsInspectorMock {
            self.inspect_results.borrow_mut().push(result);
            self
        }
    }

    #[derive(Default)]
    pub struct DnsModifierFactoryMock {
        make_results: RefCell<Vec<Option<Box<dyn DnsInspector>>>>,
    }

    impl DnsInspectorFactory for DnsModifierFactoryMock {
        fn make(&self) -> Option<Box<dyn DnsInspector>> {
            self.make_results.borrow_mut().remove(0)
        }
    }

    impl DnsModifierFactoryMock {
        pub fn new() -> DnsModifierFactoryMock {
            DnsModifierFactoryMock {
                make_results: RefCell::new(vec![]),
            }
        }

        pub fn make_result(self, result: Option<Box<dyn DnsInspector>>) -> DnsModifierFactoryMock {
            self.make_results.borrow_mut().push(result);
            self
        }
    }

    #[test]
    fn everything_in_defaults_is_properly_constructed() {
        let result = SetupReporterReal::get_default_params();

        assert_eq!(result.is_empty(), false, "{:?}", result); // if we don't have any defaults, let's get rid of all this
        result.into_iter().for_each(|(name, value)| {
            assert_eq!(name, value.name);
            assert_eq!(value.status, Default);
        });
    }

    #[test]
    fn some_items_are_censored_from_defaults() {
        let result = SetupReporterReal::get_default_params();

        assert_eq!(result.get("ui-port"), None, "{:?}", result);
        #[cfg(target_os = "windows")]
        assert_eq!(result.get("real-user"), None, "{:?}", result);
    }

    #[test]
    fn get_modified_setup_database_populated_only_requireds_set() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_database_populated_only_requireds_set",
        );
        let db_initializer = DbInitializerReal::default();
        let conn = db_initializer
            .initialize(&home_dir, true, MigratorConfig::test_default())
            .unwrap();
        let mut config = PersistentConfigurationReal::from(conn);
        config.change_password(None, "password").unwrap();
        config.set_clandestine_port(1234).unwrap();
        config
            .set_wallet_info(
                "1111111111111111111111111111111111111111111111111111111111111111",
                "0x0000000000000000000000000000000000000000",
                "password",
            )
            .unwrap();
        config.set_gas_price(1234567890).unwrap();
        let neighbor1 = NodeDescriptor {
            encryption_public_key: PublicKey::new(b"ABCD"),
            blockchain: Blockchain::EthMainnet,
            node_addr_opt: Some(NodeAddr::new(
                &IpAddr::from_str("1.2.3.4").unwrap(),
                &[1234],
            )),
        };
        let neighbor2 = NodeDescriptor {
            encryption_public_key: PublicKey::new(b"EFGH"),
            blockchain: Blockchain::EthMainnet,
            node_addr_opt: Some(NodeAddr::new(
                &IpAddr::from_str("5.6.7.8").unwrap(),
                &[5678],
            )),
        };
        config
            .set_past_neighbors(Some(vec![neighbor1, neighbor2]), "password")
            .unwrap();
        let incoming_setup = vec![
            ("data-directory", home_dir.to_str().unwrap()),
            ("db-password", "password"),
            ("ip", "4.3.2.1"),
        ]
        .into_iter()
        .map(|(name, value)| UiSetupRequestValue::new(name, value))
        .collect_vec();
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(HashMap::new(), incoming_setup)
            .unwrap();

        let (dns_servers_str, dns_servers_status) = match DnsServers::new().computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        ) {
            Some((dss, _)) => (dss, Default),
            None => ("".to_string(), Required),
        };
        let expected_result = vec![
            ("balance-decreases-for", "2592000", Default),
            ("balance-to-decrease-from", "1000000000", Default),
            ("blockchain-service-url", "", Required),
            ("chain", DEFAULT_CHAIN.rec().literal_identifier, Default),
            ("clandestine-port", "1234", Configured),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "", Blank),
            ("crash-point", "", Blank),
            ("data-directory", home_dir.to_str().unwrap(), Set),
            ("db-password", "password", Set),
            ("dns-servers", &dns_servers_str, dns_servers_status),
            ("earning-wallet", "", Blank),
            (
                "exit-byte-rate",
                DEFAULT_RATE_PACK.exit_byte_rate.to_string().as_str(),
                Default,
            ),
            (
                "exit-service-rate",
                DEFAULT_RATE_PACK.exit_service_rate.to_string().as_str(),
                Default,
            ),
            ("gas-price", "1234567890", Default),
            ("ip", "4.3.2.1", Set),
            ("log-level", "warn", Default),
            ("mapping-protocol", "", Blank),
            ("neighborhood-mode", "standard", Default),
            (
                "neighbors",
                "masq://eth-mainnet:QUJDRA@1.2.3.4:1234,masq://eth-mainnet:RUZHSA@5.6.7.8:5678",
                Configured,
            ),
            (
                "payable-scan-interval",
                &DEFAULT_PAYABLE_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "payment-grace-before-ban",
                &DEFAULT_PAYMENT_CURVES
                    .payment_grace_before_ban_sec
                    .to_string(),
                Default,
            ),
            (
                "payment-suggested-after",
                &DEFAULT_PAYMENT_CURVES
                    .payment_suggested_after_sec
                    .to_string(),
                Default,
            ),
            (
                "pending-payment-scan-interval",
                &DEFAULT_PENDING_PAYMENT_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "permanent-debt-allowed",
                &DEFAULT_PAYMENT_CURVES
                    .permanent_debt_allowed_gwei
                    .to_string(),
                Default,
            ),
            #[cfg(not(target_os = "windows"))]
            (
                "real-user",
                &RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default,
            ),
            (
                "receivable-scan-interval",
                &DEFAULT_RECEIVABLE_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "routing-byte-rate",
                &DEFAULT_RATE_PACK.routing_byte_rate.to_string(),
                Default,
            ),
            (
                "routing-service-rate",
                &DEFAULT_RATE_PACK.routing_service_rate.to_string(),
                Default,
            ),
            (
                "unban-when-balance-below",
                &DEFAULT_PAYMENT_CURVES
                    .unban_when_balance_below_gwei
                    .to_string(),
                Default,
            ),
        ]
        .into_iter()
        .map(|(name, value, status)| {
            (
                name.to_string(),
                UiSetupResponseValue::new(name, value, status),
            )
        })
        .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    fn get_modified_setup_database_nonexistent_everything_preexistent() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_database_nonexistent_everything_preexistent",
        );
        let existing_setup = setup_cluster_from(vec![
            ("balance-decreases-for","1234",Set),
            ("balance-to-decrease-from", "50000",Set),
            ("blockchain-service-url", "https://example.com", Set),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Set),
            ("clandestine-port", "1234", Set),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677", Set),
            ("crash-point", "Message", Set),
            ("data-directory", home_dir.to_str().unwrap(), Set),
            ("db-password", "password", Set),
            ("dns-servers", "8.8.8.8", Set),
            ("earning-wallet", "0x0123456789012345678901234567890123456789", Set),
            ("exit-byte-rate","3",Set),
            ("exit-service-rate","8",Set),
            ("gas-price", "50", Set),
            ("ip", "4.3.2.1", Set),
            ("log-level", "error", Set),
            ("mapping-protocol", "pmp", Set),
            ("neighborhood-mode", "originate-only", Set),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678", Set),
            ("payable-scan-interval","150",Set),
            ("payment-grace-before-ban","1000",Set),
            ("payment-suggested-after","1000",Set),
            ("pending-payment-scan-interval","150",Set),
            ("permanent-debt-allowed","20000",Set),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga", Set),
            ("receivable-scan-interval","150",Set),
            ("routing-byte-rate","1",Set),
            ("routing-service-rate","3",Set),
            ("unban-when-balance-below","20000",Set)
        ]);
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject.get_modified_setup(existing_setup, vec![]).unwrap();

        let expected_result = vec![
            ("balance-decreases-for","1234",Set),
            ("balance-to-decrease-from", "50000",Set),
            ("blockchain-service-url", "https://example.com", Set),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Set),
            ("clandestine-port", "1234", Set),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677", Set),
            ("crash-point", "Message", Set),
            ("data-directory", home_dir.to_str().unwrap(), Set),
            ("db-password", "password", Set),
            ("dns-servers", "8.8.8.8", Set),
            ("earning-wallet", "0x0123456789012345678901234567890123456789", Set),
            ("exit-byte-rate","3",Set),
            ("exit-service-rate","8",Set),
            ("gas-price", "50", Set),
            ("ip", "4.3.2.1", Set),
            ("log-level", "error", Set),
            ("mapping-protocol", "pmp", Set),
            ("neighborhood-mode", "originate-only", Set),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678", Set),
            ("payable-scan-interval","150",Set),
            ("payment-grace-before-ban","1000",Set),
            ("payment-suggested-after","1000",Set),
            ("pending-payment-scan-interval","150",Set),
            ("permanent-debt-allowed","20000",Set),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga", Set),
            ("receivable-scan-interval","150",Set),
            ("routing-byte-rate","1",Set),
            ("routing-service-rate","3",Set),
            ("unban-when-balance-below","20000",Set)
        ].into_iter()
            .map (|(name, value, status)| (name.to_string(), UiSetupResponseValue::new(name, value, status)))
            .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    fn get_modified_setup_database_nonexistent_everything_set() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_database_nonexistent_everything_set",
        );
        let incoming_setup = vec![
            ("balance-decreases-for","1234"),
            ("balance-to-decrease-from", "50000"),
            ("blockchain-service-url", "https://example.com"),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("clandestine-port", "1234"),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677"),
            ("crash-point", "Message"),
            ("data-directory", home_dir.to_str().unwrap()),
            ("db-password", "password"),
            ("dns-servers", "8.8.8.8"),
            ("earning-wallet", "0x0123456789012345678901234567890123456789"),
            ("exit-byte-rate","3"),
            ("exit-service-rate","8"),
            ("gas-price", "50"),
            ("ip", "4.3.2.1"),
            ("log-level", "error"),
            ("mapping-protocol", "igdp"),
            ("neighborhood-mode", "originate-only"),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678"),
            ("payable-scan-interval","150"),
            ("payment-grace-before-ban","1000"),
            ("payment-suggested-after","1000"),
            ("pending-payment-scan-interval","150"),
            ("permanent-debt-allowed","20000"),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga"),
            ("receivable-scan-interval","150"),
            ("routing-byte-rate","1"),
            ("routing-service-rate","3"),
            ("unban-when-balance-below","20000")
        ].into_iter()
            .map (|(name, value)| UiSetupRequestValue::new(name, value))
            .collect_vec();
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(HashMap::new(), incoming_setup)
            .unwrap();

        let expected_result = vec![
            ("balance-decreases-for","1234",Set),
            ("balance-to-decrease-from", "50000",Set),
            ("blockchain-service-url", "https://example.com", Set),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Set),
            ("clandestine-port", "1234", Set),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677", Set),
            ("crash-point", "Message", Set),
            ("data-directory", home_dir.to_str().unwrap(), Set),
            ("db-password", "password", Set),
            ("dns-servers", "8.8.8.8", Set),
            ("earning-wallet", "0x0123456789012345678901234567890123456789", Set),
            ("exit-byte-rate","3",Set),
            ("exit-service-rate","8",Set),
            ("gas-price", "50", Set),
            ("ip", "4.3.2.1", Set),
            ("log-level", "error", Set),
            ("mapping-protocol", "igdp", Set),
            ("neighborhood-mode", "originate-only", Set),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678", Set),
            ("payable-scan-interval","150",Set),
            ("payment-grace-before-ban","1000",Set),
            ("payment-suggested-after","1000",Set),
            ("pending-payment-scan-interval","150",Set),
            ("permanent-debt-allowed","20000",Set),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga", Set),
            ("receivable-scan-interval","150",Set),
            ("routing-byte-rate","1",Set),
            ("routing-service-rate","3",Set),
            ("unban-when-balance-below","20000",Set)
        ].into_iter()
            .map (|(name, value, status)| (name.to_string(), UiSetupResponseValue::new(name, value, status)))
            .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    fn get_modified_setup_database_nonexistent_nothing_set_everything_in_environment() {
        let _guard = EnvironmentGuard::new();
        let _clap_guard = ClapGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_database_nonexistent_nothing_set_everything_in_environment",
        );
        vec![
            ("MASQ_BALANCE_DECREASES_FOR","1234"),
            ("MASQ_BALANCE_TO_DECREASE_FROM","50000"),
            ("MASQ_BLOCKCHAIN_SERVICE_URL", "https://example.com"),
            ("MASQ_CHAIN", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("MASQ_CLANDESTINE_PORT", "1234"),
            ("MASQ_CONSUMING_PRIVATE_KEY", "0011223344556677001122334455667700112233445566770011223344556677"),
            ("MASQ_CRASH_POINT", "Error"),
            ("MASQ_DATA_DIRECTORY", home_dir.to_str().unwrap()),
            ("MASQ_DB_PASSWORD", "password"),
            ("MASQ_DNS_SERVERS", "8.8.8.8"),
            ("MASQ_EARNING_WALLET", "0x0123456789012345678901234567890123456789"),
            ("MASQ_EXIT_BYTE_RATE","3"),
            ("MASQ_EXIT_SERVICE_RATE","8"),
            ("MASQ_GAS_PRICE", "50"),
            ("MASQ_IP", "4.3.2.1"),
            ("MASQ_LOG_LEVEL", "error"),
            ("MASQ_MAPPING_PROTOCOL", "pmp"),
            ("MASQ_NEIGHBORHOOD_MODE", "originate-only"),
            ("MASQ_NEIGHBORS", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678"),
            ("MASQ_PAYABLE_SCAN_INTERVAL", "150"),
            ("MASQ_PAYMENT_GRACE_BEFORE_BAN","1000"),
            ("MASQ_PAYMENT_SUGGESTED_AFTER","1000"),
            ("MASQ_PENDING_PAYMENT_SCAN_INTERVAL","150"),
            ("MASQ_PERMANENT_DEBT_ALLOWED","20000"),
            #[cfg(not(target_os = "windows"))]
            ("MASQ_REAL_USER", "9999:9999:booga"),
            ("MASQ_RECEIVABLE_SCAN_INTERVAL","150"),
            ("MASQ_ROUTING_BYTE_RATE","1"),
            ("MASQ_ROUTING_SERVICE_RATE","3"),
            ("MASQ_UNBAN_WHEN_BALANCE_BELOW","20000")
        ].into_iter()
            .for_each (|(name, value)| std::env::set_var (name, value));
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let params = vec![];
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject.get_modified_setup(HashMap::new(), params).unwrap();

        let expected_result = vec![
            ("balance-decreases-for","1234",Configured),
            ("balance-to-decrease-from", "50000",Configured),
            ("blockchain-service-url", "https://example.com", Configured),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Configured),
            ("clandestine-port", "1234", Configured),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677", Configured),
            ("crash-point", "Error", Configured),
            ("data-directory", home_dir.to_str().unwrap(), Configured),
            ("db-password", "password", Configured),
            ("dns-servers", "8.8.8.8", Configured),
            ("earning-wallet", "0x0123456789012345678901234567890123456789", Configured),
            ("exit-byte-rate","3",Configured),
            ("exit-service-rate","8",Configured),
            ("gas-price", "50", Configured),
            ("ip", "4.3.2.1", Configured),
            ("log-level", "error", Configured),
            ("mapping-protocol", "pmp", Configured),
            ("neighborhood-mode", "originate-only", Configured),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678", Configured),
            ("payable-scan-interval","150",Configured),
            ("payment-grace-before-ban","1000",Configured),
            ("payment-suggested-after","1000",Configured),
            ("pending-payment-scan-interval","150",Configured),
            ("permanent-debt-allowed","20000",Configured),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga", Configured),
            ("receivable-scan-interval","150",Configured),
            ("routing-byte-rate","1",Configured),
            ("routing-service-rate","3",Configured),
            ("unban-when-balance-below","20000",Configured)
        ].into_iter()
            .map (|(name, value, status)| (name.to_string(), UiSetupResponseValue::new(name, value, status)))
            .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    // NOTE: This test achieves what it's designed for--to demonstrate that loading a different
    // config file changes the setup in the database properly--but the scenario it's built on is
    // misleading. You can't change a database from one chain to another, because in so doing all
    // its wallet addresses, balance amounts, and transaction numbers would be invalidated.
    fn switching_config_files_changes_setup() {
        let _ = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "switching_config_files_changes_setup",
        );
        let data_root = home_dir.join("data_root");
        let mainnet_dir = data_root
            .join("MASQ")
            .join(DEFAULT_CHAIN.rec().literal_identifier);
        {
            std::fs::create_dir_all(mainnet_dir.clone()).unwrap();
            let mut config_file = File::create(mainnet_dir.join("config.toml")).unwrap();
            config_file
                .write_all(b"blockchain-service-url = \"https://www.mainnet.com\"\n")
                .unwrap();
            config_file
                .write_all(b"clandestine-port = \"7788\"\n")
                .unwrap();
            config_file.write_all(b"consuming-private-key = \"00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF\"\n").unwrap();
            config_file.write_all(b"crash-point = \"None\"\n").unwrap();
            config_file
                .write_all(b"routing-byte-rate = \"1\"\n")
                .unwrap();
            config_file
                .write_all(b"routing-service-rate = \"3\"\n")
                .unwrap();
            config_file.write_all(b"exit-byte-rate = \"3\"\n").unwrap();
            config_file
                .write_all(b"exit-service-rate = \"7\"\n")
                .unwrap();
            config_file
                .write_all(b"dns-servers = \"5.6.7.8\"\n")
                .unwrap();
            config_file
                .write_all(b"earning-wallet = \"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n")
                .unwrap();
            config_file.write_all(b"gas-price = \"77\"\n").unwrap();
            config_file.write_all(b"log-level = \"trace\"\n").unwrap();
            config_file
                .write_all(b"mapping-protocol = \"pcp\"\n")
                .unwrap();
            config_file
                .write_all(b"neighborhood-mode = \"zero-hop\"\n")
                .unwrap();
        }
        let ropsten_dir = data_root
            .join("MASQ")
            .join(TEST_DEFAULT_CHAIN.rec().literal_identifier);
        {
            std::fs::create_dir_all(ropsten_dir.clone()).unwrap();
            let mut config_file = File::create(ropsten_dir.join("config.toml")).unwrap();
            config_file
                .write_all(b"blockchain-service-url = \"https://www.ropsten.com\"\n")
                .unwrap();
            config_file
                .write_all(b"clandestine-port = \"8877\"\n")
                .unwrap();
            // NOTE: You can't really change consuming-private-key without starting a new database
            config_file.write_all(b"consuming-private-key = \"FFEEDDCCBBAA99887766554433221100FFEEDDCCBBAA99887766554433221100\"\n").unwrap();
            config_file.write_all(b"crash-point = \"None\"\n").unwrap();
            config_file
                .write_all(b"db-password = \"ropstenPassword\"\n")
                .unwrap();
            config_file
                .write_all(
                    format!(
                        "routing-byte-rate = \"{}\"\n",
                        DEFAULT_RATE_PACK.routing_byte_rate
                    )
                    .as_bytes(),
                )
                .unwrap();
            config_file
                .write_all(
                    format!(
                        "routing-service-rate = \"{}\"\n",
                        DEFAULT_RATE_PACK.routing_service_rate
                    )
                    .as_bytes(),
                )
                .unwrap();
            config_file
                .write_all(
                    format!(
                        "exit-byte-rate = \"{}\"\n",
                        DEFAULT_RATE_PACK.exit_byte_rate
                    )
                    .as_bytes(),
                )
                .unwrap();
            config_file
                .write_all(
                    format!(
                        "exit-service-rate = \"{}\"\n",
                        DEFAULT_RATE_PACK.exit_service_rate
                    )
                    .as_bytes(),
                )
                .unwrap();
            config_file
                .write_all(b"dns-servers = \"8.7.6.5\"\n")
                .unwrap();
            // NOTE: You can't really change consuming-private-key without starting a new database
            config_file
                .write_all(b"earning-wallet = \"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n")
                .unwrap();
            config_file.write_all(b"gas-price = \"88\"\n").unwrap();
            config_file.write_all(b"log-level = \"debug\"\n").unwrap();
            config_file
                .write_all(b"mapping-protocol = \"pmp\"\n")
                .unwrap();
            config_file
                .write_all(b"neighborhood-mode = \"zero-hop\"\n")
                .unwrap();
        }
        let subject = SetupReporterReal::new(Box::new(
            DirsWrapperMock::new()
                .home_dir_result(Some(home_dir.clone()))
                .data_dir_result(Some(data_root.clone())),
        ));
        let params = vec![UiSetupRequestValue::new(
            "chain",
            DEFAULT_CHAIN.rec().literal_identifier,
        )];
        let existing_setup = subject.get_modified_setup(HashMap::new(), params).unwrap();
        let params = vec![UiSetupRequestValue::new(
            "chain",
            TEST_DEFAULT_CHAIN.rec().literal_identifier,
        )];

        let result = subject.get_modified_setup(existing_setup, params).unwrap();

        let expected_result = vec![
            (
                "balance-decreases-for",
                DEFAULT_PAYMENT_CURVES
                    .balance_decreases_for_sec
                    .to_string()
                    .as_str(),
                Default,
            ),
            (
                "balance-to-decrease-from",
                DEFAULT_PAYMENT_CURVES
                    .balance_to_decrease_from_gwei
                    .to_string()
                    .as_str(),
                Default,
            ),
            (
                "blockchain-service-url",
                "https://www.ropsten.com",
                Configured,
            ),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Set),
            ("clandestine-port", "8877", Configured),
            ("config-file", "config.toml", Default),
            (
                "consuming-private-key",
                "FFEEDDCCBBAA99887766554433221100FFEEDDCCBBAA99887766554433221100",
                Configured,
            ),
            ("crash-point", "None", Configured),
            (
                "data-directory",
                &ropsten_dir.to_string_lossy().to_string(),
                Default,
            ),
            ("db-password", "ropstenPassword", Configured),
            ("dns-servers", "8.7.6.5", Configured),
            (
                "earning-wallet",
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                Configured,
            ),
            (
                "exit-byte-rate",
                &DEFAULT_RATE_PACK.exit_byte_rate.to_string(),
                Configured,
            ),
            (
                "exit-service-rate",
                &DEFAULT_RATE_PACK.exit_service_rate.to_string(),
                Configured,
            ),
            ("gas-price", "88", Configured),
            ("ip", "", Blank),
            ("log-level", "debug", Configured),
            ("mapping-protocol", "pmp", Configured),
            ("neighborhood-mode", "zero-hop", Configured),
            ("neighbors", "", Blank),
            (
                "payable-scan-interval",
                &DEFAULT_PAYABLE_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "payment-grace-before-ban",
                &DEFAULT_PAYMENT_CURVES
                    .payment_grace_before_ban_sec
                    .to_string(),
                Default,
            ),
            (
                "payment-suggested-after",
                &DEFAULT_PAYMENT_CURVES
                    .payment_suggested_after_sec
                    .to_string(),
                Default,
            ),
            (
                "pending-payment-scan-interval",
                &DEFAULT_PENDING_PAYMENT_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "permanent-debt-allowed",
                &DEFAULT_PAYMENT_CURVES
                    .permanent_debt_allowed_gwei
                    .to_string(),
                Default,
            ),
            #[cfg(not(target_os = "windows"))]
            (
                "real-user",
                &crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(subject.dirs_wrapper.as_ref())
                    .to_string(),
                Default,
            ),
            (
                "receivable-scan-interval",
                &DEFAULT_RECEIVABLE_SCAN_INTERVAL.to_string(),
                Default,
            ),
            (
                "routing-byte-rate",
                &DEFAULT_RATE_PACK.routing_byte_rate.to_string(),
                Configured,
            ),
            (
                "routing-service-rate",
                &DEFAULT_RATE_PACK.routing_service_rate.to_string(),
                Configured,
            ),
            (
                "unban-when-balance-below",
                &DEFAULT_PAYMENT_CURVES
                    .unban_when_balance_below_gwei
                    .to_string(),
                Default,
            ),
        ]
        .into_iter()
        .map(|(name, value, status)| {
            (
                name.to_string(),
                UiSetupResponseValue::new(name, value, status),
            )
        })
        .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    //TODO we should change the name of this test - there are required values included, maybe 'all but fundamentals'?
    fn get_modified_setup_database_nonexistent_all_but_requireds_cleared() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_database_nonexistent_all_but_requireds_cleared",
        );
        vec![
            ("MASQ_BALANCE_DECREASES_FOR","1234"),
            ("MASQ_BALANCE_TO_DECREASE_FROM","50000"),
            ("MASQ_BLOCKCHAIN_SERVICE_URL", "https://example.com"),
            ("MASQ_CHAIN", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("MASQ_CLANDESTINE_PORT", "1234"),
            ("MASQ_CONSUMING_PRIVATE_KEY", "0011223344556677001122334455667700112233445566770011223344556677"),
            ("MASQ_CRASH_POINT", "Panic"),
            ("MASQ_DATA_DIRECTORY", home_dir.to_str().unwrap()),
            ("MASQ_DB_PASSWORD", "password"),
            ("MASQ_DNS_SERVERS", "8.8.8.8"),
            ("MASQ_EARNING_WALLET", "0x0123456789012345678901234567890123456789"),
            ("MASQ_EXIT_BYTE_RATE","3"),
            ("MASQ_EXIT_SERVICE_RATE","8"),
            ("MASQ_GAS_PRICE", "50"),
            ("MASQ_IP", "4.3.2.1"),
            ("MASQ_LOG_LEVEL", "error"),
            ("MASQ_MAPPING_PROTOCOL", "pcp"),
            ("MASQ_NEIGHBORHOOD_MODE", "originate-only"),
            ("MASQ_NEIGHBORS", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678"),
            ("MASQ_PAYABLE_SCAN_INTERVAL", "150"),
            ("MASQ_PAYMENT_GRACE_BEFORE_BAN","1000"),
            ("MASQ_PAYMENT_SUGGESTED_AFTER","1000"),
            ("MASQ_PENDING_PAYMENT_SCAN_INTERVAL","150"),
            ("MASQ_PERMANENT_DEBT_ALLOWED","20000"),
            #[cfg(not(target_os = "windows"))]
            ("MASQ_REAL_USER", "9999:9999:booga"),
            ("MASQ_RECEIVABLE_SCAN_INTERVAL","150"),
            ("MASQ_ROUTING_BYTE_RATE","1"),
            ("MASQ_ROUTING_SERVICE_RATE","3"),
            ("MASQ_UNBAN_WHEN_BALANCE_BELOW","20000")
        ].into_iter()
            .for_each (|(name, value)| std::env::set_var (name, value));
        let params = vec![
            "balance-decreases-for",
            "balance-to-decrease-from",
            "blockchain-service-url",
            "clandestine-port",
            "config-file",
            "consuming-private-key",
            "crash-point",
            "data-directory",
            "db-password",
            "dns-servers",
            "earning-wallet",
            "exit-byte-rate",
            "exit-service-rate",
            "gas-price",
            "ip",
            "log-level",
            "mapping-protocol",
            "neighborhood-mode",
            "neighbors",
            "payable-scan-interval",
            "payment-grace-before-ban",
            "payment-suggested-after",
            "pending-payment-scan-interval",
            "permanent-debt-allowed",
            #[cfg(not(target_os = "windows"))]
            "real-user",
            "receivable-scan-interval",
            "routing-byte-rate",
            "routing-service-rate",
            "unban-when-balance-below",
        ]
        .into_iter()
        .map(|name| UiSetupRequestValue::clear(name))
        .collect_vec();
        let existing_setup = setup_cluster_from(vec![
            ("balance-decreases-for", "4321", Set),
            ("balance-to-decrease-from", "66666", Set),
            ("blockchain-service-url", "https://booga.com", Set),
            ("clandestine-port", "4321", Set),
            (
                "consuming-private-key",
                "7766554433221100776655443322110077665544332211007766554433221100",
                Set,
            ),
            ("crash-point", "Message", Set),
            ("data-directory", "booga", Set),
            ("db-password", "drowssap", Set),
            ("dns-servers", "4.4.4.4", Set),
            (
                "earning-wallet",
                "0x9876543210987654321098765432109876543210",
                Set,
            ),
            ("exit-byte-rate", "13", Set),
            ("exit-service-rate", "28", Set),
            ("gas-price", "5", Set),
            ("ip", "1.2.3.4", Set),
            ("log-level", "error", Set),
            ("mapping-protocol", "pcp", Set),
            ("neighborhood-mode", "consume-only", Set),
            (
                "neighbors",
                "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@9.10.11.12:9101",
                Set,
            ),
            ("payable-scan-interval", "111", Set),
            ("payment-grace-before-ban", "777", Set),
            ("payment-suggested-after", "987", Set),
            ("pending-payment-scan-interval", "111", Set),
            ("permanent-debt-allowed", "123456", Set),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "6666:6666:agoob", Set),
            ("receivable-scan-interval", "111", Set),
            ("routing-byte-rate", "10", Set),
            ("routing-service-rate", "30", Set),
            ("unban-when-balance-below", "123456", Set),
        ]);
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject.get_modified_setup(existing_setup, params).unwrap();

        let expected_result = vec![
            ("balance-decreases-for","1234",Configured),
            ("balance-to-decrease-from", "50000",Configured),
            ("blockchain-service-url", "https://example.com", Configured),
            ("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier, Configured),
            ("clandestine-port", "1234", Configured),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "0011223344556677001122334455667700112233445566770011223344556677", Configured),
            ("crash-point", "Panic", Configured),
            ("data-directory", home_dir.to_str().unwrap(), Configured),
            ("db-password", "password", Configured),
            ("dns-servers", "8.8.8.8", Configured),
            (
                "earning-wallet",
                "0x0123456789012345678901234567890123456789",
                Configured,
            ),
            ("exit-byte-rate","3",Configured),
            ("exit-service-rate","8",Configured),
            ("gas-price", "50", Configured),
            ("ip", "4.3.2.1", Configured),
            ("log-level", "error", Configured),
            ("mapping-protocol", "pcp", Configured),
            ("neighborhood-mode", "originate-only", Configured),
            ("neighbors", "masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@1.2.3.4:1234,masq://eth-ropsten:MTIzNDU2Nzg5MTEyMzQ1Njc4OTIxMjM0NTY3ODkzMTI@5.6.7.8:5678", Configured),
            ("payable-scan-interval","150",Configured),
            ("payment-grace-before-ban","1000",Configured),
            ("payment-suggested-after","1000",Configured),
            ("pending-payment-scan-interval","150",Configured),
            ("permanent-debt-allowed","20000",Configured),
            #[cfg(not(target_os = "windows"))]
            ("real-user", "9999:9999:booga", Configured),
            ("receivable-scan-interval","150",Configured),
            ("routing-byte-rate","1",Configured),
            ("routing-service-rate","3",Configured),
            ("unban-when-balance-below","20000",Configured)
        ]
        .into_iter()
        .map(|(name, value, status)| {
            (
                name.to_string(),
                UiSetupResponseValue::new(name, value, status),
            )
        })
        .collect_vec();
        let presentable_result = result
            .into_iter()
            .sorted_by_key(|(k, _)| k.clone())
            .collect_vec();
        assert_eq!(presentable_result, expected_result);
    }

    #[test]
    fn get_modified_setup_data_directory_depends_on_new_chain_on_success() {
        let _guard = EnvironmentGuard::new();
        let base_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_data_directory_depends_on_new_chain_on_success",
        );
        let current_data_dir = base_dir
            .join("MASQ")
            .join(DEFAULT_CHAIN.rec().literal_identifier);
        let existing_setup = setup_cluster_from(vec![
            ("neighborhood-mode", "zero-hop", Set),
            ("chain", DEFAULT_CHAIN.rec().literal_identifier, Default),
            (
                "data-directory",
                &current_data_dir.to_string_lossy().to_string(),
                Default,
            ),
            (
                "real-user",
                &crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default,
            ),
        ]);
        let incoming_setup = vec![("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier)]
            .into_iter()
            .map(|(name, value)| UiSetupRequestValue::new(name, value))
            .collect_vec();
        let base_data_dir = base_dir.join("data_dir");
        let expected_data_directory = base_data_dir
            .join("MASQ")
            .join(TEST_DEFAULT_CHAIN.rec().literal_identifier);
        let dirs_wrapper = Box::new(
            DirsWrapperMock::new()
                .data_dir_result(Some(base_data_dir))
                .home_dir_result(Some(base_dir)),
        );
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(existing_setup, incoming_setup)
            .unwrap();

        let actual_data_directory = PathBuf::from(&result.get("data-directory").unwrap().value);
        assert_eq!(actual_data_directory, expected_data_directory);
    }

    #[test]
    fn get_modified_setup_data_directory_depends_on_new_chain_on_error() {
        let _guard = EnvironmentGuard::new();
        let base_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_data_directory_depends_on_new_chain_on_error",
        );
        let current_data_dir = base_dir
            .join("MASQ")
            .join(DEFAULT_CHAIN.rec().literal_identifier);
        let existing_setup = setup_cluster_from(vec![
            ("blockchain-service-url", "", Required),
            ("chain", DEFAULT_CHAIN.rec().literal_identifier, Default),
            ("clandestine-port", "7788", Default),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "", Blank),
            (
                "data-directory",
                &current_data_dir.to_string_lossy().to_string(),
                Default,
            ),
            ("db-password", "", Required),
            ("dns-servers", "1.1.1.1", Default),
            (
                "earning-wallet",
                "0x47fb8671db83008d382c2e6ea67fa377378c0cea",
                Default,
            ),
            ("gas-price", "1", Default),
            ("ip", "1.2.3.4", Set),
            ("log-level", "warn", Default),
            ("neighborhood-mode", "originate-only", Set),
            ("neighbors", "", Blank),
            (
                "real-user",
                &crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default,
            ),
        ]);
        let incoming_setup = vec![("chain", TEST_DEFAULT_CHAIN.rec().literal_identifier)]
            .into_iter()
            .map(|(name, value)| UiSetupRequestValue::new(name, value))
            .collect_vec();
        let base_data_dir = base_dir.join("data_dir");
        let expected_data_directory = base_data_dir
            .join("MASQ")
            .join(TEST_DEFAULT_CHAIN.rec().literal_identifier);
        let dirs_wrapper = Box::new(
            DirsWrapperMock::new()
                .data_dir_result(Some(base_data_dir))
                .home_dir_result(Some(base_dir)),
        );
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(existing_setup, incoming_setup)
            .err()
            .unwrap()
            .0;

        let actual_data_directory = PathBuf::from(&result.get("data-directory").unwrap().value);
        assert_eq!(actual_data_directory, expected_data_directory);
    }

    #[test]
    fn get_modified_setup_data_directory_trying_to_blank_chain_out_on_error() {
        //by blanking the original chain the default value is set to its place
        let _guard = EnvironmentGuard::new();
        let base_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_data_directory_trying_to_blank_chain_out_on_error",
        );
        let current_data_dir = base_dir
            .join("MASQ")
            .join(BlockChain::PolyMumbai.rec().literal_identifier); //not a default
        let existing_setup = setup_cluster_from(vec![
            ("blockchain-service-url", "", Required),
            (
                "chain",
                BlockChain::PolyMumbai.rec().literal_identifier,
                Set,
            ),
            ("clandestine-port", "7788", Default),
            ("config-file", "config.toml", Default),
            ("consuming-private-key", "", Blank),
            (
                "data-directory",
                &current_data_dir.to_string_lossy().to_string(),
                Default,
            ),
            ("db-password", "", Required),
            ("dns-servers", "1.1.1.1", Default),
            (
                "earning-wallet",
                "0x47fb8671db83008d382c2e6ea67fa377378c0cea",
                Default,
            ),
            ("gas-price", "1", Default),
            ("ip", "1.2.3.4", Set),
            ("log-level", "warn", Default),
            ("neighborhood-mode", "originate-only", Set),
            ("neighbors", "", Blank),
            (
                "real-user",
                &crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default,
            ),
        ]);
        let incoming_setup = vec![UiSetupRequestValue::clear("chain")];
        let base_data_dir = base_dir.join("data_dir");
        let expected_chain = DEFAULT_CHAIN.rec().literal_identifier;
        let expected_data_directory = base_data_dir
            .join("MASQ")
            .join(DEFAULT_CHAIN.rec().literal_identifier);
        let dirs_wrapper = Box::new(
            DirsWrapperMock::new()
                .data_dir_result(Some(base_data_dir))
                .home_dir_result(Some(base_dir)),
        );
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(existing_setup, incoming_setup)
            .err()
            .unwrap()
            .0;

        let actual_chain = &result.get("chain").unwrap().value;
        assert_eq!(actual_chain, expected_chain);
        let actual_data_directory = PathBuf::from(&result.get("data-directory").unwrap().value);
        assert_eq!(actual_data_directory, expected_data_directory);
    }

    #[test]
    fn get_modified_setup_does_not_support_database_migration() {
        let data_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_setup_does_not_support_database_migration",
        );
        let conn =
            bring_db_of_version_0_back_to_life_and_return_connection(&data_dir.join(DATABASE_FILE));
        let dao = ConfigDaoReal::new(Box::new(ConnectionWrapperReal::new(conn)));
        let schema_version_before = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, "0");
        let existing_setup = setup_cluster_from(vec![
            ("chain", DEFAULT_CHAIN.rec().literal_identifier, Default),
            (
                "data-directory",
                &data_dir.to_string_lossy().to_string(),
                Set,
            ),
            (
                "real-user",
                &crate::bootstrapper::RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default,
            ),
        ]);
        let incoming_setup = vec![("ip", "1.2.3.4")]
            .into_iter()
            .map(|(name, value)| UiSetupRequestValue::new(name, value))
            .collect_vec();
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let _ = subject
            .get_modified_setup(existing_setup, incoming_setup)
            .unwrap();

        let schema_version_after = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, schema_version_after)
    }

    #[test]
    fn get_modified_blanking_something_that_should_not_be_blanked_fails_properly() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "get_modified_blanking_something_that_shouldnt_be_blanked_fails_properly",
        );
        let existing_setup = setup_cluster_from(vec![
            ("data-directory", home_dir.to_str().unwrap(), Set),
            ("neighborhood-mode", "originate-only", Set),
            (
                "neighbors",
                "masq://eth-mainnet:gBviQbjOS3e5ReFQCvIhUM3i02d1zPleo1iXg_EN6zQ@86.75.30.9:5542",
                Set,
            ),
        ]);
        let incoming_setup = vec![UiSetupRequestValue::clear("neighbors")];
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(existing_setup, incoming_setup)
            .err()
            .unwrap();

        assert_eq!(
            result.0.get("neighbors").unwrap().clone(),
            UiSetupResponseValue::new(
                "neighbors",
                "masq://eth-mainnet:gBviQbjOS3e5ReFQCvIhUM3i02d1zPleo1iXg_EN6zQ@86.75.30.9:5542",
                Set
            )
        );
    }

    #[test]
    fn run_configuration_without_existing_database_with_config_dao_null_to_use() {
        let data_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "run_configuration_without_existing_database_with_config_dao_null_to_use",
        );
        let conn =
            bring_db_of_version_0_back_to_life_and_return_connection(&data_dir.join(DATABASE_FILE));
        conn.execute("update config set value = 55 where name = 'gas_price'", [])
            .unwrap();
        let dao = ConfigDaoReal::new(Box::new(ConnectionWrapperReal::new(conn)));
        let updated_gas_price = dao.get("gas_price").unwrap().value_opt.unwrap();
        assert_eq!(updated_gas_price, "55");
        let schema_version_before = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, "0");
        let multi_config =
            make_simplified_multi_config(["--data-directory", data_dir.to_str().unwrap()]);
        let dirs_wrapper = make_pre_populated_mocked_directory_wrapper();
        let subject = SetupReporterReal::new(Box::new(dirs_wrapper));

        let ((bootstrapper_config, mut persistent_config), _) =
            subject.run_configuration(&multi_config, &data_dir);

        assert_ne!(bootstrapper_config.blockchain_bridge_config.gas_price, 55); //asserting negation
        let schema_version_after = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, schema_version_after);
        persistent_config.set_gas_price(66).unwrap();
        //if this had contained ConfigDaoReal the setting would've worked
        let gas_price = persistent_config.gas_price().unwrap();
        assert_ne!(gas_price, 66);
    }

    #[test]
    fn run_configuration_suppresses_db_migration_that_is_why_it_offers_just_config_dao_null_to_use()
    {
        let data_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "run_configuration_suppresses_db_migration_that_is_why_it_offers_just_config_dao_null_to_use",
        );
        let conn =
            bring_db_of_version_0_back_to_life_and_return_connection(&data_dir.join(DATABASE_FILE));
        conn.execute("update config set value = 55 where name = 'gas_price'", [])
            .unwrap();
        let dao = ConfigDaoReal::new(Box::new(ConnectionWrapperReal::new(conn)));
        let updated_gas_price = dao.get("gas_price").unwrap().value_opt.unwrap();
        assert_eq!(updated_gas_price, "55");
        let schema_version_before = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, "0");
        let multi_config =
            make_simplified_multi_config(["--data-directory", data_dir.to_str().unwrap()]);
        let dirs_wrapper = make_pre_populated_mocked_directory_wrapper();
        let subject = SetupReporterReal::new(Box::new(dirs_wrapper));

        let ((bootstrapper_config, mut persistent_config), _) =
            subject.run_configuration(&multi_config, &data_dir);

        assert_ne!(bootstrapper_config.blockchain_bridge_config.gas_price, 55); //asserting negation
        let schema_version_after = dao.get("schema_version").unwrap().value_opt.unwrap();
        assert_eq!(schema_version_before, schema_version_after);
        persistent_config.set_gas_price(66).unwrap();
        //if this had contained ConfigDaoReal the setting would've worked
        let gas_price = persistent_config.gas_price().unwrap();
        assert_ne!(gas_price, 66);
    }

    #[test]
    fn calculate_fundamentals_with_only_environment() {
        let _guard = EnvironmentGuard::new();
        vec![
            ("MASQ_CHAIN", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("MASQ_DATA_DIRECTORY", "env_dir"),
            ("MASQ_REAL_USER", "9999:9999:booga"),
        ]
        .into_iter()
        .for_each(|(name, value)| std::env::set_var(name, value));
        let setup = setup_cluster_from(vec![]);

        let (real_user_opt, data_directory_opt, chain) =
            SetupReporterReal::calculate_fundamentals(&DirsWrapperReal {}, &setup).unwrap();

        assert_eq!(
            real_user_opt,
            Some(crate::bootstrapper::RealUser::new(
                Some(9999),
                Some(9999),
                Some(PathBuf::from("booga"))
            ))
        );
        assert_eq!(data_directory_opt, Some(PathBuf::from("env_dir")));
        assert_eq!(chain, TEST_DEFAULT_CHAIN);
    }

    #[test]
    fn calculate_fundamentals_with_environment_and_obsolete_setup() {
        let _guard = EnvironmentGuard::new();
        vec![
            ("MASQ_CHAIN", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("MASQ_DATA_DIRECTORY", "env_dir"),
            ("MASQ_REAL_USER", "9999:9999:booga"),
        ]
        .into_iter()
        .for_each(|(name, value)| std::env::set_var(name, value));
        let setup = setup_cluster_from(vec![
            ("chain", "dev", Configured),
            ("data-directory", "setup_dir", Default),
            ("real-user", "1111:1111:agoob", Configured),
        ]);

        let (real_user_opt, data_directory_opt, chain) =
            SetupReporterReal::calculate_fundamentals(&DirsWrapperReal {}, &setup).unwrap();

        assert_eq!(
            real_user_opt,
            Some(crate::bootstrapper::RealUser::new(
                Some(9999),
                Some(9999),
                Some(PathBuf::from("booga"))
            ))
        );
        assert_eq!(data_directory_opt, Some(PathBuf::from("env_dir")));
        assert_eq!(chain, TEST_DEFAULT_CHAIN);
    }

    #[test]
    fn calculate_fundamentals_with_environment_and_overriding_setup() {
        let _guard = EnvironmentGuard::new();
        vec![
            ("MASQ_CHAIN", TEST_DEFAULT_CHAIN.rec().literal_identifier),
            ("MASQ_DATA_DIRECTORY", "env_dir"),
            ("MASQ_REAL_USER", "9999:9999:booga"),
        ]
        .into_iter()
        .for_each(|(name, value)| std::env::set_var(name, value));
        let setup = setup_cluster_from(vec![
            ("chain", "dev", Set),
            ("data-directory", "setup_dir", Set),
            ("real-user", "1111:1111:agoob", Set),
        ]);

        let (real_user_opt, data_directory_opt, chain) =
            SetupReporterReal::calculate_fundamentals(&DirsWrapperReal {}, &setup).unwrap();

        assert_eq!(
            real_user_opt,
            Some(crate::bootstrapper::RealUser::new(
                Some(1111),
                Some(1111),
                Some(PathBuf::from("agoob"))
            ))
        );
        assert_eq!(data_directory_opt, Some(PathBuf::from("setup_dir")));
        assert_eq!(chain, Blockchain::from("dev"));
    }

    #[test]
    fn calculate_fundamentals_with_setup_and_no_environment() {
        let _guard = EnvironmentGuard::new();
        vec![]
            .into_iter()
            .for_each(|(name, value): (&str, &str)| std::env::set_var(name, value));
        let setup = setup_cluster_from(vec![
            ("chain", "dev", Configured),
            ("data-directory", "setup_dir", Default),
            ("real-user", "1111:1111:agoob", Configured),
        ]);

        let (real_user_opt, data_directory_opt, chain) =
            SetupReporterReal::calculate_fundamentals(&DirsWrapperReal {}, &setup).unwrap();

        assert_eq!(
            real_user_opt,
            Some(crate::bootstrapper::RealUser::new(
                Some(1111),
                Some(1111),
                Some(PathBuf::from("agoob"))
            ))
        );
        assert_eq!(data_directory_opt, None);
        assert_eq!(chain, Blockchain::from("dev"));
    }

    #[test]
    fn calculate_fundamentals_with_neither_setup_nor_environment() {
        let _guard = EnvironmentGuard::new();
        vec![]
            .into_iter()
            .for_each(|(name, value): (&str, &str)| std::env::set_var(name, value));
        let setup = setup_cluster_from(vec![]);

        let (real_user_opt, data_directory_opt, chain) =
            SetupReporterReal::calculate_fundamentals(&DirsWrapperReal {}, &setup).unwrap();

        assert_eq!(
            real_user_opt,
            Some(
                crate::bootstrapper::RealUser::new(None, None, None).populate(&DirsWrapperReal {})
            )
        );
        assert_eq!(data_directory_opt, None);
        assert_eq!(chain, DEFAULT_CHAIN);
    }

    #[test]
    fn blanking_a_parameter_with_a_default_produces_that_default() {
        let _guard = EnvironmentGuard::new();
        let home_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "blanking_a_parameter_with_a_default_produces_that_default",
        );
        let dirs_wrapper = Box::new(DirsWrapperReal);
        let subject = SetupReporterReal::new(dirs_wrapper);

        let result = subject
            .get_modified_setup(
                HashMap::new(),
                vec![
                    UiSetupRequestValue::new(
                        "data-directory",
                        &home_dir.to_string_lossy().to_string(),
                    ),
                    UiSetupRequestValue::new("ip", "1.2.3.4"),
                    UiSetupRequestValue::clear("chain"),
                ],
            )
            .unwrap();

        let actual_chain = result.get("chain").unwrap();
        assert_eq!(
            actual_chain,
            &UiSetupResponseValue::new("chain", DEFAULT_CHAIN.rec().literal_identifier, Default)
        );
    }

    #[test]
    fn choose_uisrv_chooses_higher_priority_incoming_over_lower_priority_existing() {
        let existing = UiSetupResponseValue::new("name", "existing", Configured);
        let incoming = UiSetupResponseValue::new("name", "incoming", Set);

        let result = SetupReporterReal::choose_uisrv(&existing, &incoming);

        assert_eq!(result, &incoming);
    }

    #[test]
    fn choose_uisrv_chooses_higher_priority_existing_over_lower_priority_incoming() {
        let existing = UiSetupResponseValue::new("name", "existing", Set);
        let incoming = UiSetupResponseValue::new("name", "incoming", Configured);

        let result = SetupReporterReal::choose_uisrv(&existing, &incoming);

        assert_eq!(result, &existing);
    }

    #[test]
    fn choose_uisrv_chooses_incoming_over_existing_for_equal_priority() {
        let existing = UiSetupResponseValue::new("name", "existing", Set);
        let incoming = UiSetupResponseValue::new("name", "incoming", Set);

        let result = SetupReporterReal::choose_uisrv(&existing, &incoming);

        assert_eq!(result, &incoming);
    }

    #[test]
    fn config_file_not_specified_and_nonexistent() {
        let data_directory = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_not_specified_and_nonexistent",
        );
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new(
                "data-directory",
                &data_directory.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();
        let subject = SetupReporterReal::new(Box::new(DirsWrapperReal {}));

        let result = subject
            .calculate_configured_setup(&setup, &data_directory)
            .0;

        assert_eq!(
            result.get("config-file").unwrap().value,
            "config.toml".to_string()
        );
        assert_eq!(
            result.get("gas-price").unwrap().value,
            GasPrice {}
                .computed_default(
                    &BootstrapperConfig::new(),
                    &make_persistent_config_real_with_config_dao_null(),
                    &None
                )
                .unwrap()
                .0
        );
    }

    #[test]
    fn config_file_not_specified_but_exists() {
        let data_directory = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_not_specified_but_exists",
        );
        {
            let config_file_path = data_directory.join("config.toml");
            let mut config_file = File::create(config_file_path).unwrap();
            config_file.write_all(b"gas-price = \"10\"\n").unwrap();
        }
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new(
                "data-directory",
                &data_directory.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();

        let result = SetupReporterReal::new(Box::new(DirsWrapperReal {}))
            .calculate_configured_setup(&setup, &data_directory)
            .0;

        assert_eq!(result.get("gas-price").unwrap().value, "10".to_string());
    }

    #[test]
    fn config_file_has_relative_directory_that_exists_in_data_directory() {
        let data_directory = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_has_relative_directory_that_exists_in_data_directory",
        );
        {
            let config_file_dir = data_directory.join("booga");
            std::fs::create_dir_all(&config_file_dir).unwrap();
            let config_file_path = config_file_dir.join("special.toml");
            let mut config_file = File::create(config_file_path).unwrap();
            config_file.write_all(b"gas-price = \"10\"\n").unwrap();
        }
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new("config-file", "booga/special.toml", Set),
            UiSetupResponseValue::new(
                "data-directory",
                &data_directory.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();
        let subject = SetupReporterReal::new(Box::new(DirsWrapperReal {}));

        let result = subject
            .calculate_configured_setup(&setup, &data_directory)
            .0;

        assert_eq!(result.get("gas-price").unwrap().value, "10".to_string());
    }

    #[test]
    fn config_file_has_relative_directory_that_does_not_exist_in_data_directory() {
        let data_directory = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_has_relative_directory_that_does_not_exist_in_data_directory",
        );
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new("config-file", "booga/special.toml", Set),
            UiSetupResponseValue::new(
                "data-directory",
                &data_directory.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();
        let subject = SetupReporterReal::new(Box::new(DirsWrapperReal {}));

        let result = subject
            .calculate_configured_setup(&setup, &data_directory)
            .1
            .unwrap();

        assert_eq!(result.param_errors[0].parameter, "config-file");
        assert_string_contains(&result.param_errors[0].reason, "Are you sure it exists?");
    }

    #[test]
    fn config_file_has_absolute_path_to_file_that_exists() {
        let data_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_has_absolute_path_to_file_that_exists",
        )
        .canonicalize()
        .unwrap();
        let config_file_dir = data_dir.join("data_dir").join("my_config_file");
        std::fs::create_dir_all(&config_file_dir).unwrap();
        let config_file_path = config_file_dir.join("special.toml");
        {
            let mut config_file = File::create(config_file_path.clone()).unwrap();
            config_file.write_all(b"gas-price = \"10\"\n").unwrap();
        }
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new(
                "config-file",
                &config_file_path.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();
        let subject = SetupReporterReal::new(Box::new(DirsWrapperReal {}));

        let result = subject.calculate_configured_setup(&setup, &data_dir).0;

        eprintln!("{:?}", result);
        assert_eq!(result.get("gas-price").unwrap().value, "10".to_string());
    }

    #[test]
    fn config_file_has_absolute_path_to_file_that_does_not_exist() {
        let config_file_dir = ensure_node_home_directory_exists(
            "setup_reporter",
            "config_file_has_absolute_path_to_file_that_does_not_exist",
        );
        let config_file_dir = config_file_dir.canonicalize().unwrap();
        let config_file_path = config_file_dir.join("nonexistent.toml");
        let wrapper = DirsWrapperReal {};
        let data_directory = wrapper
            .data_dir()
            .unwrap()
            .join("MASQ")
            .join(DEFAULT_CHAIN.rec().literal_identifier);
        let setup = vec![
            // no config-file setting
            UiSetupResponseValue::new("neighborhood-mode", "zero-hop", Set),
            UiSetupResponseValue::new(
                "config-file",
                &config_file_path.to_string_lossy().to_string(),
                Set,
            ),
        ]
        .into_iter()
        .map(|uisrv| (uisrv.name.clone(), uisrv))
        .collect();
        let subject = SetupReporterReal::new(Box::new(DirsWrapperReal {}));

        let result = subject
            .calculate_configured_setup(&setup, &data_directory)
            .1
            .unwrap();

        assert_eq!(result.param_errors[0].parameter, "config-file");
        assert_string_contains(&result.param_errors[0].reason, "Are you sure it exists?");
    }

    #[test]
    fn chain_computed_default() {
        let subject = Chain {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(
            result,
            Some((DEFAULT_CHAIN.rec().literal_identifier.to_string(), Default))
        );
    }

    #[test]
    fn clandestine_port_computed_default_present() {
        let persistent_config =
            PersistentConfigurationMock::new().clandestine_port_result(Ok(1234));
        let subject = ClandestinePort {};

        let result =
            subject.computed_default(&BootstrapperConfig::new(), &persistent_config, &None);

        assert_eq!(result, Some(("1234".to_string(), Configured)))
    }

    #[test]
    fn clandestine_port_database_field_error() {
        let subject = ClandestinePort {};
        let persistent_config = PersistentConfigurationMock::new()
            .clandestine_port_result(Err(PersistentConfigError::NotPresent));

        let result =
            subject.computed_default(&BootstrapperConfig::new(), &persistent_config, &None);

        assert_eq!(result, None)
    }

    #[test]
    fn data_directory_computed_default() {
        let real_user = RealUser::new(None, None, None).populate(&DirsWrapperReal {});
        let expected = data_directory_from_context(
            &DirsWrapperReal {},
            &real_user,
            &None,
            Blockchain::EthMainnet,
        )
        .to_string_lossy()
        .to_string();
        let mut config = BootstrapperConfig::new();
        config.real_user = real_user;
        config.blockchain_bridge_config.chain = Blockchain::from("eth-mainnet");

        let subject = DataDirectory::default();

        let result = subject.computed_default(
            &config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some((expected, Default)))
    }

    #[test]
    fn dns_servers_computed_default_does_not_exist_when_platform_is_not_recognized() {
        let factory = DnsModifierFactoryMock::new().make_result(None);
        let mut subject = DnsServers::new();
        subject.factory = Box::new(factory);

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None)
    }

    #[test]
    fn dns_servers_computed_default_does_not_exist_when_dns_is_subverted() {
        let modifier = DnsInspectorMock::new()
            .inspect_result(Ok(vec![IpAddr::from_str("127.0.0.1").unwrap()]));
        let factory = DnsModifierFactoryMock::new().make_result(Some(Box::new(modifier)));
        let mut subject = DnsServers::new();
        subject.factory = Box::new(factory);

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None)
    }

    #[test]
    fn dns_servers_computed_default_does_not_exist_when_dns_inspection_fails() {
        init_test_logging();
        let modifier =
            DnsInspectorMock::new().inspect_result(Err(DnsInspectionError::NotConnected));
        let factory = DnsModifierFactoryMock::new().make_result(Some(Box::new(modifier)));
        let mut subject = DnsServers::new();
        subject.factory = Box::new(factory);

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None);
        TestLogHandler::new().exists_log_containing("WARN: SetupReporter: Error inspecting DNS settings: This system does not appear to be connected to a network");
    }

    #[test]
    fn dns_servers_computed_default_does_not_exist_when_dns_inspection_returns_no_addresses() {
        let modifier = DnsInspectorMock::new().inspect_result(Ok(vec![]));
        let factory = DnsModifierFactoryMock::new().make_result(Some(Box::new(modifier)));
        let mut subject = DnsServers::new();
        subject.factory = Box::new(factory);

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None)
    }

    #[test]
    fn dns_servers_computed_default_exists_when_dns_inspection_succeeds() {
        let modifier = DnsInspectorMock::new().inspect_result(Ok(vec![
            IpAddr::from_str("192.168.0.1").unwrap(),
            IpAddr::from_str("8.8.8.8").unwrap(),
        ]));
        let factory = DnsModifierFactoryMock::new().make_result(Some(Box::new(modifier)));
        let mut subject = DnsServers::new();
        subject.factory = Box::new(factory);

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("192.168.0.1,8.8.8.8".to_string(), Default)))
    }

    #[test]
    fn earning_wallet_computed_default_with_everything_configured_is_still_none() {
        let mut config = BootstrapperConfig::new();
        config.earning_wallet = Wallet::new("command-line address");
        let persistent_config = PersistentConfigurationMock::new()
            .earning_wallet_address_result(Ok(Some("persistent address".to_string())));
        let subject = EarningWallet {};

        let result = subject.computed_default(&config, &persistent_config, &None);

        assert_eq!(result, None)
    }

    #[test]
    fn earning_wallet_computed_default_with_nothing_configured_is_still_none() {
        let config = BootstrapperConfig::new();
        let subject = EarningWallet {};

        let result = subject.computed_default(
            &config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None)
    }

    #[test]
    fn gas_price_computed_default_present() {
        let mut bootstrapper_config = BootstrapperConfig::new();
        bootstrapper_config.blockchain_bridge_config.gas_price = 57;
        let subject = GasPrice {};

        let result = subject.computed_default(
            &bootstrapper_config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("57".to_string(), Default)))
    }

    #[test]
    fn gas_price_computed_default_absent() {
        let subject = GasPrice {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("1".to_string(), Default)))
    }

    #[test]
    fn ip_computed_default_when_automap_works_and_neighborhood_mode_is_not_standard() {
        let subject = Ip {};
        let mut config = BootstrapperConfig::new();
        config.neighborhood_config.mode = crate::sub_lib::neighborhood::NeighborhoodMode::ZeroHop;

        let result = subject.computed_default(
            &config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("".to_string(), Blank)));
    }

    #[test]
    fn ip_computed_default_when_neighborhood_mode_is_standard() {
        let subject = Ip {};
        let mut config = BootstrapperConfig::new();
        config.neighborhood_config.mode = crate::sub_lib::neighborhood::NeighborhoodMode::Standard(
            NodeAddr::new(&IpAddr::from_str("5.6.7.8").unwrap(), &[1234]),
            vec![],
            DEFAULT_RATE_PACK,
        );

        let result = subject.computed_default(
            &config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("5.6.7.8".to_string(), Set)));
    }

    #[test]
    fn ip_computed_default_when_automap_does_not_work_and_neighborhood_mode_is_not_standard() {
        let subject = Ip {};
        let mut config = BootstrapperConfig::new();
        config.neighborhood_config.mode = crate::sub_lib::neighborhood::NeighborhoodMode::ZeroHop;

        let result = subject.computed_default(
            &config,
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("".to_string(), Blank)));
    }

    #[test]
    fn log_level_computed_default() {
        let subject = LogLevel {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("warn".to_string(), Default)))
    }

    #[test]
    fn mapping_protocol_is_just_blank_if_no_data_in_database_and_unspecified_on_command_line() {
        let subject = MappingProtocol {};
        let persistent_config =
            PersistentConfigurationMock::default().mapping_protocol_result(Ok(None));

        let result =
            subject.computed_default(&BootstrapperConfig::new(), &persistent_config, &None);

        assert_eq!(result, None)
    }

    #[test]
    fn mapping_protocol_is_configured_if_data_in_database_and_no_command_line() {
        let subject = MappingProtocol {};
        let persistent_config = PersistentConfigurationMock::default()
            .mapping_protocol_result(Ok(Some(AutomapProtocol::Pmp)));
        let bootstrapper_config = BootstrapperConfig::new();

        let result = subject.computed_default(&bootstrapper_config, &persistent_config, &None);

        assert_eq!(result, Some(("pmp".to_string(), Configured)))
    }

    #[test]
    fn mapping_protocol_is_configured_if_no_database_but_bootstrapper_config_contains_some_value() {
        let subject = MappingProtocol {};
        let persistent_config =
            PersistentConfigurationMock::default().mapping_protocol_result(Ok(None));
        let mut bootstrapper_config = BootstrapperConfig::new();
        bootstrapper_config.mapping_protocol_opt = Some(AutomapProtocol::Pcp);

        let result = subject.computed_default(&bootstrapper_config, &persistent_config, &None);

        assert_eq!(result, Some(("pcp".to_string(), Configured)))
    }

    #[test]
    fn neighborhood_mode_computed_default() {
        let subject = NeighborhoodMode {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, Some(("standard".to_string(), Default)))
    }

    #[test]
    fn neighbors_computed_default_persistent_config_present_password_present_values_present() {
        let past_neighbors_params_arc = Arc::new(Mutex::new(vec![]));
        let persistent_config = PersistentConfigurationMock::new()
            .past_neighbors_params(&past_neighbors_params_arc)
            .past_neighbors_result(Ok(Some(vec![
                NodeDescriptor::try_from((
                    main_cryptde(),
                    "masq://eth-mainnet:MTEyMjMzNDQ1NTY2Nzc4ODExMjIzMzQ0NTU2Njc3ODg@1.2.3.4:1234",
                ))
                .unwrap(),
                NodeDescriptor::try_from((
                    main_cryptde(),
                    "masq://eth-mainnet:ODg3NzY2NTU0NDMzMjIxMTg4Nzc2NjU1NDQzMzIyMTE@4.3.2.1:4321",
                ))
                .unwrap(),
            ])));
        let subject = Neighbors {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &persistent_config,
            &Some("password".to_string()),
        );

        assert_eq! (result, Some (("masq://eth-mainnet:MTEyMjMzNDQ1NTY2Nzc4ODExMjIzMzQ0NTU2Njc3ODg@1.2.3.4:1234,masq://eth-mainnet:ODg3NzY2NTU0NDMzMjIxMTg4Nzc2NjU1NDQzMzIyMTE@4.3.2.1:4321".to_string(), Configured)));
        let past_neighbors_params = past_neighbors_params_arc.lock().unwrap();
        assert_eq!(*past_neighbors_params, vec!["password".to_string()])
    }

    #[test]
    fn neighbors_computed_default_persistent_config_present_password_present_values_absent() {
        let past_neighbors_params_arc = Arc::new(Mutex::new(vec![]));
        let persistent_config = PersistentConfigurationMock::new()
            .past_neighbors_params(&past_neighbors_params_arc)
            .past_neighbors_result(Ok(None));
        let subject = Neighbors {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &persistent_config,
            &Some("password".to_string()),
        );

        assert_eq!(result, None);
        let past_neighbors_params = past_neighbors_params_arc.lock().unwrap();
        assert_eq!(*past_neighbors_params, vec!["password".to_string()])
    }

    #[test]
    fn neighbors_computed_default_persistent_config_present_password_present_but_with_err() {
        let past_neighbors_params_arc = Arc::new(Mutex::new(vec![]));
        let persistent_config = PersistentConfigurationMock::new()
            .past_neighbors_params(&past_neighbors_params_arc)
            .past_neighbors_result(Err(PersistentConfigError::PasswordError));
        let subject = Neighbors {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &persistent_config,
            &Some("password".to_string()),
        );

        assert_eq!(result, None);
        let past_neighbors_params = past_neighbors_params_arc.lock().unwrap();
        assert_eq!(*past_neighbors_params, vec!["password".to_string()])
    }

    #[test]
    fn neighbors_computed_default_persistent_config_present_password_absent() {
        // absence of configured result will cause panic if past_neighbors is called
        let persistent_config = PersistentConfigurationMock::new();
        let subject = Neighbors {};

        let result =
            subject.computed_default(&BootstrapperConfig::new(), &persistent_config, &None);

        assert_eq!(result, None);
    }

    #[test]
    fn neighbors_computed_default_absent() {
        let subject = Neighbors {};

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn real_user_computed_default() {
        let subject = crate::daemon::setup_reporter::RealUser::default();

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(
            result,
            Some((
                RealUser::new(None, None, None)
                    .populate(&DirsWrapperReal {})
                    .to_string(),
                Default
            ))
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn real_user_computed_default() {
        let subject = crate::daemon::setup_reporter::RealUser::default();

        let result = subject.computed_default(
            &BootstrapperConfig::new(),
            &make_persistent_config_real_with_config_dao_null(),
            &None,
        );

        assert_eq!(result, None);
    }

    macro_rules! rate_pack_computed_default_all_values_equal_to_default  {
        ($parameter_name: ident,$subject: ident) => {
            let subject = $subject{};
            let mut bootstrapper_config = BootstrapperConfig::new();
            let mut rate_pack = DEFAULT_RATE_PACK;
            rate_pack.$parameter_name = rate_pack.$parameter_name;
            let neighbor = NodeDescriptor::try_from((main_cryptde(),
                "masq://eth-mainnet:AgMEBQ@2.3.4.5:2345"))
                .unwrap();
            let neighborhood_mode = NeighborhoodModeEnum::Standard(
                NodeAddr::new(&localhost(), &[1234]),
                vec![neighbor],
                DEFAULT_RATE_PACK,
            );
            bootstrapper_config.neighborhood_config.mode = neighborhood_mode;

            let result = subject.computed_default(
                  &bootstrapper_config,
                  &make_persistent_config_real_with_config_dao_null(),
                  &None,
            );

            assert_eq!(
                result,
                Some((
                    DEFAULT_RATE_PACK.$parameter_name.to_string(),
                    Default
                ))
            )
        }
    }

    proc_macros::triple_test_computed_default!(routing_byte_rate, u64, 2);

    #[test]
    fn routing_byte_rate_computed_default_all_values_equal_to_default(){
        rate_pack_computed_default_all_values_equal_to_default!(routing_byte_rate,RoutingByteRate);
    }

    proc_macros::triple_test_computed_default!(routing_service_rate, u64, 2);

    proc_macros::triple_test_computed_default!(exit_service_rate, u64, 2);

    proc_macros::triple_test_computed_default!(exit_byte_rate, u64, 2);

    proc_macros::triple_test_computed_default!(pending_payment_scan_interval, u64, 3);

    proc_macros::triple_test_computed_default!(payable_scan_interval, u64, 3);

    proc_macros::triple_test_computed_default!(receivable_scan_interval, u64, 3);

    proc_macros::triple_test_computed_default!(balance_decreases_for_sec, u64, 1);

    proc_macros::triple_test_computed_default!(balance_to_decrease_from_gwei, u64, 1);

    proc_macros::triple_test_computed_default!(payment_suggested_after_sec, u64, 1);

    proc_macros::triple_test_computed_default!(payment_grace_before_ban_sec, u64, 1);

    proc_macros::triple_test_computed_default!(permanent_debt_allowed_gwei, u64, 1);

    proc_macros::triple_test_computed_default!(unban_when_balance_below_gwei, u64, 1);

    #[test]
    fn computed_default_rate_pack_when_value_from_bootstrapper_config_is_none(){
        //this None actually means that a neighborhood mode different from 'standard' or 'originate-only'
        //has been set
        let bc_value = None;
        let persistent_config_value = 5555;

        let result = computed_default_rate_pack(&bc_value, persistent_config_value,DEFAULT_RATE_PACK.routing_service_rate );

        assert_eq!(result,None)
    }

    #[test]
    fn exit_service_rate_computed_default_value_when_neighborhood_mode_is_originate_only(){
        let subject = ExitServiceRate{};
        let mut config = BootstrapperConfig::new();
        let mut rate_pack = rate_pack(123);
        rate_pack.exit_service_rate = DEFAULT_RATE_PACK.exit_service_rate;
        let cryptde = main_cryptde();
        let originate_only = NeighborhoodModeEnum::OriginateOnly(
            vec![NodeDescriptor::from((
                cryptde.public_key(),
                &NodeAddr::new(&IpAddr::from_str("1.2.3.4").unwrap(), &[1234]),
                Blockchain::EthRopsten,
                cryptde,
            ))],
            rate_pack,
        );
        config.neighborhood_config.mode = originate_only;
        let persistent_config = PersistentConfigurationReal::new(Box::new(ConfigDaoNull::default()));

        let result = subject.computed_default(&config,&persistent_config,&None);

        assert_eq!(result,Some((DEFAULT_RATE_PACK.exit_service_rate.to_string(),Default)))
    }

    fn verify_requirements(
        subject: &dyn ValueRetriever,
        param_name: &str,
        value_predictions: Vec<(&str, bool)>,
    ) {
        value_predictions
            .into_iter()
            .for_each(|(param_value, prediction)| {
                let params = vec![(
                    param_name.to_string(),
                    UiSetupResponseValue::new(param_name, param_value, Set),
                )]
                .into_iter()
                .collect::<SetupCluster>();

                let result = subject.is_required(&params);

                assert_eq!(result, prediction, "{:?}", params);
            })
    }

    fn verify_needed_for_blockchain(subject: &dyn ValueRetriever) {
        verify_requirements(
            subject,
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", true),
            ],
        );
    }

    #[test]
    fn ip_requirements() {
        verify_requirements(
            &Ip {},
            "neighborhood-mode",
            vec![
                ("standard", false),
                ("zero-hop", false),
                ("originate-only", false),
                ("consume-only", false),
            ],
        );
    }

    #[test]
    fn dnsservers_requirements() {
        verify_requirements(
            &DnsServers::new(),
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", true),
                ("originate-only", true),
                ("consume-only", false),
            ],
        );
    }

    #[test]
    fn neighbors_requirements() {
        verify_requirements(
            &Neighbors {},
            "neighborhood-mode",
            vec![
                ("standard", false),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", true),
            ],
        );
    }

    #[test]
    fn blockchain_requirements() {
        verify_needed_for_blockchain(&BlockchainServiceUrl {});
        verify_needed_for_blockchain(&DbPassword {});
        verify_needed_for_blockchain(&GasPrice {});
    }

    #[test]
    fn routing_byte_rate_requirements() {
        verify_requirements(
            &RoutingByteRate {},
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", false),
            ],
        );
    }

    #[test]
    fn routing_service_rate_requirements() {
        verify_requirements(
            &RoutingServiceRate {},
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", false),
            ],
        );
    }

    #[test]
    fn exit_byte_rate_requirements() {
        verify_requirements(
            &ExitByteRate {},
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", false),
            ],
        );
    }

    #[test]
    fn exit_service_rate_requirements() {
        verify_requirements(
            &ExitServiceRate {},
            "neighborhood-mode",
            vec![
                ("standard", true),
                ("zero-hop", false),
                ("originate-only", true),
                ("consume-only", false),
            ],
        );
    }

    //those marked with asterisk are factually irrelevant because values for them are always present
    //in the database and can be reset to defaults only (sort of factory settings)
    #[test]
    fn dumb_requirements() {
        let params = HashMap::new();
        assert_eq!(BalanceToDecreaseFromGwei {}.is_required(&params), true); //*
        assert_eq!(BlockchainServiceUrl {}.is_required(&params), true);
        assert_eq!(Chain {}.is_required(&params), true);
        assert_eq!(ClandestinePort {}.is_required(&params), true);
        assert_eq!(ConfigFile {}.is_required(&params), false);
        assert_eq!(ConsumingPrivateKey {}.is_required(&params), false);
        assert_eq!(DataDirectory::default().is_required(&params), true);
        assert_eq!(DbPassword {}.is_required(&params), true);
        assert_eq!(DnsServers::new().is_required(&params), true);
        assert_eq!(EarningWallet {}.is_required(&params), false);
        assert_eq!(GasPrice {}.is_required(&params), true);
        assert_eq!(Ip {}.is_required(&params), false);
        assert_eq!(LogLevel {}.is_required(&params), true);
        assert_eq!(MappingProtocol {}.is_required(&params), false);
        assert_eq!(NeighborhoodMode {}.is_required(&params), true);
        assert_eq!(Neighbors {}.is_required(&params), true);
        assert_eq!(PayableScanInterval {}.is_required(&params), true); //*
        assert_eq!(PaymentSuggestedAfterSec {}.is_required(&params), true); //*
        assert_eq!(PendingPaymentScanInterval {}.is_required(&params), true); //*
        assert_eq!(PermanentDebtAllowedGwei {}.is_required(&params), true); //*
        assert_eq!(ReceivableScanInterval {}.is_required(&params), true); //*
        assert_eq!(
            crate::daemon::setup_reporter::RealUser::default().is_required(&params),
            false
        );
        assert_eq!(UnbanWhenBalanceBelowGwei {}.is_required(&params), true) //*
    }

    #[test]
    fn value_retrievers_know_their_names() {
        assert_eq!(
            BlockchainServiceUrl {}.value_name(),
            "blockchain-service-url"
        );
        assert_eq!(Chain {}.value_name(), "chain");
        assert_eq!(ClandestinePort {}.value_name(), "clandestine-port");
        assert_eq!(ConfigFile {}.value_name(), "config-file");
        assert_eq!(ConsumingPrivateKey {}.value_name(), "consuming-private-key");
        assert_eq!(DataDirectory::default().value_name(), "data-directory");
        assert_eq!(DbPassword {}.value_name(), "db-password");
        assert_eq!(DnsServers::new().value_name(), "dns-servers");
        assert_eq!(EarningWallet {}.value_name(), "earning-wallet");
        assert_eq!(GasPrice {}.value_name(), "gas-price");
        assert_eq!(Ip {}.value_name(), "ip");
        assert_eq!(LogLevel {}.value_name(), "log-level");
        assert_eq!(MappingProtocol {}.value_name(), "mapping-protocol");
        assert_eq!(NeighborhoodMode {}.value_name(), "neighborhood-mode");
        assert_eq!(Neighbors {}.value_name(), "neighbors");
        assert_eq!(
            crate::daemon::setup_reporter::RealUser::default().value_name(),
            "real-user"
        );
    }
}
