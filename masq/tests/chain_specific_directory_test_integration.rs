// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use std::thread;
use std::time::Duration;
use crate::utils::{DaemonProcess, MasqProcess};
use masq_lib::utils::find_free_port;

mod utils;

#[test]
fn ensure_data_directory_has_specific_chain_direcotry_within() {
    let port = find_free_port();
    let daemon_handle = DaemonProcess::new().start(port.clone());
    thread::sleep(Duration::from_millis(3000));

    let masq_handle = MasqProcess::new().start_noninteractive(vec![
        "--ui-port",
        *&port.to_string().to_owned().as_str(),
        "setup"
    ]);

    thread::sleep(Duration::from_millis(1000));

    let (stdout, _stderr, _exit_code) = masq_handle.stop();

    if stdout.contains("MASQ/polygon-mainnet/MASQ/polygon-mainnet Default") {
        panic!("Wrong directory: MASQ/polygon-mainnet/MASQ/polygon-mainnet when Default");
    }

    let mut masq_handle2 = MasqProcess::new().start_interactive(port, true);

    let mut stdin_handle = masq_handle2.create_stdin_handle();

    stdin_handle.type_command("setup --data-directory /Users/vojtechparkan/masqhome");

    thread::sleep(Duration::from_millis(1000));

    stdin_handle.type_command("exit");

    let (stdout2, stderr2, exit_code2) = masq_handle2.stop();

    if !stdout2.contains("masqhome/polygon-mainnet                    Set") {
        panic!("Wrong directory: MASQ/polygon-mainnet/MASQ/polygon-mainnet when Default");
    }

    daemon_handle.kill();
}