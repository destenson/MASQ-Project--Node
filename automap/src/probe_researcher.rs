// Copyright (c) 2019-2021, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::automap_core_functions::{remove_firewall_hole, remove_permanent_firewall_hole};
use crate::comm_layer::Transactor;
use rand::{thread_rng, Rng};
use std::cell::Cell;
use std::fmt::{Display, Formatter};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::Add;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fmt, thread};

//so far, println!() is safer for testing, with immediate feedback
#[allow(clippy::result_unit_err)]
pub fn close_exposed_port(
    _stdout: &mut dyn Write,
    _stderr: &mut dyn Write,
    params: NextSectionShifter,
) -> Result<(), ()> {
    println!("Preparation for closing the forwarded port");
    match params.method {
        Method::Pmp | Method::Pcp | Method::Igdp(false) => {
            remove_firewall_hole(_stdout, _stderr, params)
        }
        Method::Igdp(true) => remove_permanent_firewall_hole(_stdout, _stderr, params),
    }
}

#[derive(PartialEq, Debug)]
pub enum Method {
    Pmp,
    Pcp,
    Igdp(bool),
}

impl Display for Method {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Method::Pmp => write!(f, "PMP protocol"),
            Method::Pcp => write!(f, "PCP protocol"),
            Method::Igdp(_flag) => write!(f, "IGDP protocol"),
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn prepare_router_or_report_failure(
    test_pcp: Box<dyn FnOnce() -> Result<(IpAddr, u16, Box<dyn Transactor>), String>>,
    test_pmp: Box<dyn FnOnce() -> Result<(IpAddr, u16, Box<dyn Transactor>), String>>,
    test_igdp: Box<dyn FnOnce() -> Result<(IpAddr, u16, Box<dyn Transactor>, bool), String>>,
) -> Result<NextSectionShifter, Vec<String>> {
    let mut collector: Vec<String> = vec![];
    match test_pcp() {
        Ok((ip, port, transactor)) => {
            return Ok(NextSectionShifter {
                method: Method::Pcp,
                ip,
                port,
                transactor,
            })
        }
        Err(e) => collector.push(e),
    };
    match test_pmp() {
        Ok((ip, port, transactor)) => {
            return Ok(NextSectionShifter {
                method: Method::Pmp,
                ip,
                port,
                transactor,
            })
        }
        Err(e) => collector.push(e),
    };
    match test_igdp() {
        Ok((ip, port, transactor, permanent)) => {
            return Ok(NextSectionShifter {
                method: Method::Igdp(permanent),
                ip,
                port,
                transactor,
            })
        }
        Err(e) => collector.push(e),
    };
    if collector.len() == 3 {
        Err(collector)
    } else {
        panic!("shouldn't happen")
    }
}

pub struct NextSectionShifter {
    pub method: Method,
    pub ip: IpAddr,
    pub port: u16,
    pub transactor: Box<dyn Transactor>,
}

fn deploy_background_listener(
    port: u16,
    expected_nonce: u16,
    timeout_millis: u64,
) -> JoinHandle<Result<(), std::io::Error>> {
    let listener =
        TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port)).unwrap();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now().add(Duration::from_millis(timeout_millis));
        let mut stream = loop {
            if Instant::now() >= deadline {
                return Err(std::io::Error::from(ErrorKind::TimedOut));
            }
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock => (),
                Err(e) => return Err(e),
            }
        };
        let mut buf = [0u8; 2];
        stream
            .set_read_timeout(Some(Duration::from_millis(timeout_millis)))
            .unwrap();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break (Err(std::io::Error::from(ErrorKind::BrokenPipe))),
                Ok(_) => {
                    let actual_nonce = ((buf[0] as u16) << 8) | (buf[1] as u16);
                    if actual_nonce == expected_nonce {
                        break Ok(());
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    break (Err(std::io::Error::from(ErrorKind::TimedOut)))
                }
                Err(e) => break Err(e),
            }
        }
    })
}

pub fn probe_researcher(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    server_address: SocketAddr,
    params: &mut NextSectionShifter,
) -> bool {
    write!(
        stdout,
        "Test of a port forwarded by using {} is starting. \n\n",
        params.method
    )
    .expect("write failed");

    let success_sign = Cell::new(false);
    evaluate_research(stdout, stderr, server_address, params, &success_sign);

    stderr.flush().expect("failed to flush stdout");
    stdout.flush().expect("failed to flush stderr");

    success_sign.take()
}

fn evaluate_research(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    server_address: SocketAddr,
    params: &mut NextSectionShifter,
    success_sign: &Cell<bool>,
) {
    let nonce = generate_nonce();
    let thread_handle = deploy_background_listener(params.port, nonce, 3000);
    let http_request = format!(
        "GET /probe_request?ip={}&port={}&nonce={} HTTP/1.1\r\n\r\n",
        params.ip, params.port, nonce
    );
    let mut connection: TcpStream = match TcpStream::connect(server_address) {
        Ok(conn) => conn,
        Err(e) => {
            write!(
                stderr,
                "We couldn't connect to the \
             http server: {:?}. Test is terminating.",
                e
            )
            .expect("writing failed");
            return;
        }
    };
    match connection.write_all(http_request.as_bytes()) {
        Ok(_) => (),
        Err(_) => {
            stderr
                .write_all(
                    b"Sending an http request to \
                 the server failed. Test is terminating.",
                )
                .expect("writing failed");
            return;
        } // untested but safe
    }
    let mut buffer = [0u8; 1024];
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("unsuccessful during setting nonblocking");
    let mut server_responded = false;
    match connection.read(&mut buffer) {
        Ok(length) => {
            stdout
                .write_all(&buffer[..length])
                .expect("writing server response failed");
            server_responded = true;
        }
        Err(e) if e.kind() == ErrorKind::TimedOut => stderr
            .write_all(b"Request to the server was sent but no response came back. ")
            .expect("writing to stderr failed"),
        Err(e) => write!(
            stderr,
            "Request to the server was sent but reading the response failed: {:?} ",
            e
        )
        .expect("write!ing to stderr failed"),
    };
    if !server_responded {
        return;
    }
    match thread_handle.join() {
        Ok(Ok(_)) => {
            stdout
                .write_all(b"\n\nThe received nonce was evaluated to be a match; test passed")
                .expect("write_all failed");
            success_sign.set(true);
        }
        Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => stdout
            .write_all(b"\n\nThe probe detector detected no incoming probe")
            .expect("write_all failed"),
        Ok(Err(e)) => write!(stdout, "\n\nThe probe detector ran into a problem: {:?}", e)
            .expect("write! failed"),
        Err(e) => {
            write!(stderr, "\n\nThe probe detector panicked: {:?}", e).expect("write_all failed")
        }
    }
}

fn generate_nonce() -> u16 {
    let mut rnd = thread_rng();
    rnd.gen_range(1000, 9999)
}

#[cfg(test)]
mod tests {
    use crate::comm_layer::pmp::PmpTransactor;
    use crate::comm_layer::Transactor;
    use crate::probe_researcher::{
        deploy_background_listener, generate_nonce, prepare_router_or_report_failure,
        probe_researcher, Method, NextSectionShifter,
    };
    use masq_lib::utils::{find_free_port, localhost};
    use std::io::{ErrorKind, IoSlice, Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::str::FromStr;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn prepare_router_or_report_failure_retrieves_ip() {
        let result = prepare_router_or_report_failure(
            Box::new(mock_router_common_test_unsuccessful),
            Box::new(mock_router_common_test_finding_ip_and_doing_mapping),
            Box::new(mock_router_igdp_test_unsuccessful),
        );

        //sadly not all of those types implementing Transactor can implement PartialEq each
        assert!(result.is_ok());
        let unwrapped_result = result.unwrap();
        assert_eq!(unwrapped_result.ip, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
        assert_eq!(unwrapped_result.method, Method::Pmp);
        assert_eq!(unwrapped_result.port, 4444);
        //proof that I received an implementer of Transactor
        let _downcast_value: &PmpTransactor =
            unwrapped_result.transactor.as_any().downcast_ref().unwrap();
    }

    #[test]
    fn prepare_router_or_report_failure_reports_of_accumulated_errors() {
        let result = prepare_router_or_report_failure(
            Box::new(mock_router_common_test_unsuccessful),
            Box::new(mock_router_common_test_unsuccessful),
            Box::new(mock_router_igdp_test_unsuccessful),
        );

        let expected_message = String::from("Test ended unsuccessfully");

        assert_eq!(
            result.err().unwrap(),
            vec![
                expected_message.clone(),
                expected_message.clone(),
                expected_message
            ]
        )
    }

    //TODO remove this note: macOS Ok(Err(Kind(TimedOut)))
    #[test]
    fn deploy_background_listener_with_good_probe_works() {
        let port = find_free_port();

        let handle = deploy_background_listener(port, 8875, 500);
        test_stream_acceptor_and_probe_8875_imitator(true, 0, port);

        let result = handle.join();
        match result {
            Ok(Ok(())) => (),
            x => panic!("Expected Ok(Ok(())), got {:?}", x),
        }
    }

    //TODO remove this note: macOS Ok(Err(Kind(TimedOut))
    #[test]
    fn deploy_background_listener_without_getting_probe_propagates_that_fact_correctly_after_connection_interrupted(
    ) {
        let port = find_free_port();

        let handle = deploy_background_listener(port, 8875, 100);
        test_stream_acceptor_and_probe_8875_imitator(false, 0, port);

        let result = handle.join();

        #[cfg(not(taget_os = "macos"))]
        match result {
            Ok(Err(e)) if e.kind() == ErrorKind::BrokenPipe => (),
            x => panic!("Expected Ok(Err(BrokenPipe)); got {:?}", x),
        }
        #[cfg(taget_os = "macos")]
        match result {
            Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => (),
            x => panic!("Expected Ok(Err(TimeOut)); got {:?}", x),
        }
    }

    #[test]
    fn deploy_background_listener_without_getting_probe_terminates_alone_after_connection_lasts_too_long(
    ) {
        let port = find_free_port();
        let handle = deploy_background_listener(port, 8875, 200);

        test_stream_acceptor_and_probe_8875_imitator(false, 300, port);

        let result = handle.join();

        match result {
            Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => (),
            x => panic!("Expected Ok(Err(TimedOut)); got {:?}", x),
        }
    }

    #[test]
    fn deploy_background_listener_ends_its_job_after_waiting_period_for_any_connection_but_none_was_sensed(
    ) {
        let handle = deploy_background_listener(7004, 1234, 10);

        let result = handle.join();
        match result {
            Ok(Err(e)) if e.kind() == ErrorKind::TimedOut => (),
            x => panic!("Expected Ok(Err(TimedOut)), got {:?}", x),
        }
    }

    #[test]
    fn generate_nonce_works() {
        (1..100).for_each(|_| {
            let nonce = generate_nonce();
            assert!(10000 > nonce && nonce > 999)
        });
    }

    #[test]
    #[ignore]
    //server must be running so that we can get this test green!
    fn probe_researcher_works() {
        let mut stdout = MockStream::new();
        let mut stderr = MockStream::new();
        let mut parameters_transferor = NextSectionShifter {
            method: Method::Pmp,
            ip: IpAddr::V4(Ipv4Addr::from_str("127.0.0.1").unwrap()),
            port: 3545,
            transactor: Box::new(PmpTransactor::default()),
        };
        let server_address = SocketAddr::from_str("127.0.0.1:7005").unwrap();

        let result = probe_researcher(
            &mut stdout,
            &mut stderr,
            server_address,
            &mut parameters_transferor,
        );

        thread::sleep(Duration::from_secs(2));
        assert_eq!(result, true);
        assert_eq!(stdout.stream, "Test of a port forwarded by using PMP protocol is starting. \
         \n\nHTTP/1.1 200 OK\r\nContent-Length: 67\r\n\r\nconnection: success; writing: success; connection shutdown: \
         success\n\nThe received nonce was evaluated to be a match; test passed"
        );
        assert!(stderr.stream.is_empty());
        assert_eq!(stdout.flush_count, 1);
        assert_eq!(stderr.flush_count, 1);
    }

    #[test]
    fn probe_researcher_returns_failure_if_cannot_connect_to_the_http_server() {
        let mut stdout = MockStream::new();
        let mut stderr = MockStream::new();
        let port = find_free_port();
        let mut parameters_transferor = NextSectionShifter {
            method: Method::Pmp,
            ip: IpAddr::V4(Ipv4Addr::from_str("0.0.0.0").unwrap()),
            port,
            transactor: Box::new(PmpTransactor::default()),
        };
        let server_address = SocketAddr::from_str("0.0.0.0:7010").unwrap();

        let result = probe_researcher(
            &mut stdout,
            &mut stderr,
            server_address,
            &mut parameters_transferor,
        );
        assert_eq!(result, false);
        assert!(
            stderr
                .stream
                .starts_with("We couldn\'t connect to the http server: "),
            "{}",
            stderr.stream
        );
        assert!(
            stderr.stream.ends_with(". Test is terminating."),
            "{}",
            stderr.stream
        );
        assert_eq!(
            stdout.stream,
            "Test of a port forwarded by using PMP protocol is starting. \n\n"
        );
        assert_eq!(stdout.flush_count, 1);
        assert_eq!(stderr.flush_count, 1);
    }

    //TODO remove this note:  --linux: 'We couldn't connect to the http server. Test is terminating.'
    #[test]
    fn probe_researcher_sends_request_and_returns_failure_as_the_response_from_the_http_server_has_never_come_back(
    ) {
        let mut stdout = MockStream::new();
        let mut stderr = MockStream::new();

        let server_address_string = format!("127.0.0.40:{}", find_free_port());
        let server_address_clone = server_address_string.clone();
        //fake server
        thread::spawn(move || {
            let listener =
                TcpListener::bind(SocketAddr::from_str(&server_address_clone).unwrap()).unwrap();

            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_millis(1000)))
                .unwrap();
            let mut buf = [0u8; 1024];
            connection.read(&mut buf).unwrap();
            thread::sleep(Duration::from_millis(5000))
        });

        let mut parameters_transferor = NextSectionShifter {
            method: Method::Pmp,
            ip: IpAddr::V4(Ipv4Addr::from_str("0.0.0.0").unwrap()),
            port: find_free_port(),
            transactor: Box::new(PmpTransactor::default()),
        };

        let server_socket_addr = SocketAddr::from_str(&server_address_string).unwrap();

        let result = probe_researcher(
            &mut stdout,
            &mut stderr,
            server_socket_addr,
            &mut parameters_transferor,
        );
        assert_eq!(result, false);
        assert_eq!(
            stdout.stream,
            "Test of a port forwarded by using PMP protocol is starting. \n\n"
        );
        assert!(
            stderr
                .stream
                .starts_with("Request to the server was sent but "),
            "{}",
            stderr.stream
        );
        assert_eq!(stdout.flush_count, 1);
        assert_eq!(stderr.flush_count, 1);
    }

    fn mock_router_common_test_finding_ip_and_doing_mapping(
    ) -> Result<(IpAddr, u16, Box<dyn Transactor>), String> {
        Ok((
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
            4444,
            Box::new(PmpTransactor::new()),
        ))
    }

    fn mock_router_common_test_unsuccessful() -> Result<(IpAddr, u16, Box<dyn Transactor>), String>
    {
        Err(String::from("Test ended unsuccessfully"))
    }

    fn mock_router_igdp_test_unsuccessful(
    ) -> Result<(IpAddr, u16, Box<dyn Transactor>, bool), String> {
        Err(String::from("Test ended unsuccessfully"))
    }

    fn test_stream_acceptor_and_probe_8875_imitator(
        send_probe: bool,
        shutdown_delay_millis: u64,
        port: u16,
    ) {
        let mut connection = TcpStream::connect(SocketAddr::new(localhost(), port)).unwrap();
        if send_probe {
            let message = u16_to_byte_array(8875);
            connection.write_all(&message).unwrap();
        } else {
            if shutdown_delay_millis == 0 {
                connection.shutdown(Shutdown::Both).unwrap();
            } else {
                thread::sleep(Duration::from_millis(shutdown_delay_millis));
            }
        }
    }

    fn u16_to_byte_array(x: u16) -> [u8; 2] {
        let b1: u8 = ((x >> 8) & 0xff) as u8;
        let b2: u8 = (x & 0xff) as u8;
        return [b1, b2];
    }

    struct MockStream {
        stream: String,
        flush_count: u8,
    }

    impl MockStream {
        fn new() -> Self {
            Self {
                stream: String::new(),
                flush_count: 0,
            }
        }
    }

    impl Write for MockStream {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            unimplemented!()
        }

        fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
            unimplemented!()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flush_count += 1;
            Ok(())
        }

        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.stream.push_str(std::str::from_utf8(buf).unwrap());
            Ok(())
        }

        fn by_ref(&mut self) -> &mut Self
        where
            Self: Sized,
        {
            unimplemented!()
        }
    }
}