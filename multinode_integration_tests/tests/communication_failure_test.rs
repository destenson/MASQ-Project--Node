// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use masq_lib::utils::find_free_port;
use multinode_integration_tests_lib::masq_node::{MASQNode, PortSelector};
use multinode_integration_tests_lib::masq_node_cluster::MASQNodeCluster;
use multinode_integration_tests_lib::masq_real_node::NodeStartupConfigBuilder;
use node_lib::neighborhood::AccessibleGossipRecord;
use node_lib::sub_lib::cryptde::{PublicKey};
use std::convert::TryInto;
use std::net::SocketAddr;
use std::time::Duration;
use masq_lib::test_utils::utils::TEST_DEFAULT_MULTINODE_CHAIN;
use multinode_integration_tests_lib::neighborhood_constructor::construct_neighborhood;
use node_lib::json_masquerader::JsonMasquerader;
use node_lib::neighborhood::neighborhood_database::NeighborhoodDatabase;
use node_lib::neighborhood::node_record::NodeRecord;
use node_lib::sub_lib::cryptde_null::CryptDENull;
use node_lib::sub_lib::hopper::{ExpiredCoresPackage, IncipientCoresPackage, MessageType};
use node_lib::sub_lib::neighborhood::{RatePack};
use node_lib::sub_lib::proxy_client::DnsResolveFailure_0v1;
use node_lib::sub_lib::route::Route;
use node_lib::sub_lib::versioned_data::VersionedData;
use node_lib::test_utils::neighborhood_test_utils::{db_from_node, make_node_record};
use std::str::FromStr;

#[test]
#[ignore] // Should be removed by SC-811/GH-158
fn neighborhood_notified_of_newly_missing_node() {
    // Set up three-Node network, and add a mock witness Node.
    let mut cluster = MASQNodeCluster::start().unwrap();
    let chain = cluster.chain;
    let neighbor = cluster.start_real_node(
        NodeStartupConfigBuilder::standard()
            .fake_public_key(&PublicKey::new(&[1, 2, 3, 4]))
            .chain(chain)
            .build(),
    );
    let originating_node = cluster.start_real_node(
        NodeStartupConfigBuilder::standard()
            .neighbor(neighbor.node_reference())
            .fake_public_key(&PublicKey::new(&[2, 3, 4, 5]))
            .chain(chain)
            .build(),
    );
    let _staying_up_node = cluster.start_real_node(
        NodeStartupConfigBuilder::standard()
            .neighbor(neighbor.node_reference())
            .fake_public_key(&PublicKey::new(&[3, 4, 5, 6]))
            .chain(chain)
            .build(),
    );
    let disappearing_node = cluster.start_real_node(
        NodeStartupConfigBuilder::standard()
            .neighbor(neighbor.node_reference())
            .fake_public_key(&PublicKey::new(&[4, 5, 6, 7]))
            .chain(chain)
            .build(),
    );
    let witness_node = cluster
        .start_mock_node_with_public_key(vec![find_free_port()], &PublicKey::new(&[5, 6, 7, 8]));
    witness_node.transmit_debut(&originating_node).unwrap();
    let (introductions, _) = witness_node
        .wait_for_gossip(Duration::from_millis(1000))
        .unwrap();
    assert!(
        introductions.node_records.len() > 1,
        "Should have been introductions, but wasn't: {}",
        introductions.to_dot_graph(
            (
                originating_node.main_public_key(),
                &Some(originating_node.node_addr()),
            ),
            (
                witness_node.main_public_key(),
                &Some(witness_node.node_addr()),
            ),
        )
    );

    // Kill one of the Nodes--not the originating Node and not the witness Node.
    cluster.stop_node(disappearing_node.name());

    //Establish a client on the originating Node and send some ill-fated traffic.
    let mut client = originating_node.make_client(8080);
    client.send_chunk("GET http://example.com HTTP/1.1\r\n\r\n".as_bytes());

    // Now direct the witness Node to wait for Gossip about the disappeared Node.
    let (disappearance_gossip, _) = witness_node
        .wait_for_gossip(Duration::from_secs(130))
        .unwrap();

    let dot_graph = disappearance_gossip.to_dot_graph(
        (
            originating_node.main_public_key(),
            &Some(originating_node.node_addr()),
        ),
        (
            witness_node.main_public_key(),
            &Some(witness_node.node_addr()),
        ),
    );
    assert_eq!(
        3,
        disappearance_gossip.node_records.len(),
        "Should have had three records: {}",
        dot_graph
    );
    let disappearance_agrs: Vec<AccessibleGossipRecord> = disappearance_gossip.try_into().unwrap();
    let originating_node_agr = disappearance_agrs
        .into_iter()
        .find(|agr| &agr.inner.public_key == originating_node.main_public_key())
        .unwrap();
    assert!(
        !originating_node_agr
            .inner
            .neighbors
            .contains(&disappearing_node.main_public_key(),),
        "Originating Node {} should not be connected to the disappeared Node {}, but is: {}",
        originating_node.main_public_key(),
        disappearing_node.main_public_key(),
        dot_graph
    );
}

#[test]
fn dns_resolution_failure_no_longer_blacklists_exit_node_for_all_hosts() {
    let mut cluster = MASQNodeCluster::start().unwrap();
    // Make network:
    // originating_node --> relay1 --> relay2 --> cheap_exit
    //                                   |
    //                                   +--> normal_exit
    let (originating_node, relay1_mock, cheap_exit_key, normal_exit_key) = {
        let originating_node: NodeRecord = make_node_record(1234, true);
        let mut db: NeighborhoodDatabase = db_from_node(&originating_node);
        let relay1 = db.add_node(make_node_record(2345, true)).unwrap();
        let relay2 = db.add_node(make_node_record(3456, false)).unwrap();
        let mut cheap_exit_node = make_node_record(4567, false);
        let normal_exit_node = make_node_record(5678, false);
        cheap_exit_node.inner.rate_pack = cheaper_rate_pack(normal_exit_node.rate_pack(), 1);
        let cheap_exit_key = db.add_node(cheap_exit_node).unwrap();
        let normal_exit_key = db.add_node(normal_exit_node).unwrap();
        db.add_arbitrary_full_neighbor(originating_node.public_key(), &relay1);
        db.add_arbitrary_full_neighbor(&relay1, &relay2);
        db.add_arbitrary_full_neighbor(&relay2, &cheap_exit_key);
        db.add_arbitrary_full_neighbor(&relay2, &normal_exit_key);
        let (_, originating_node, mut node_map)
            = construct_neighborhood(&mut cluster, db, vec![]);
        let relay1_mock = node_map.remove(&relay1).unwrap();
        (originating_node, relay1_mock, cheap_exit_key, normal_exit_key)
    };
    let mut client = originating_node.make_client (8080);
    let masquerader = JsonMasquerader::new();
    let originating_node_cryptde = CryptDENull::from (&originating_node.main_public_key(), TEST_DEFAULT_MULTINODE_CHAIN);
    let relay1_cryptde = CryptDENull::from (&relay1_mock.main_public_key(), TEST_DEFAULT_MULTINODE_CHAIN);
    let cheap_exit_cryptde = CryptDENull::from (&cheap_exit_key, TEST_DEFAULT_MULTINODE_CHAIN);

    // This request should be routed through cheap_exit because it's cheaper
    client.send_chunk("GET / HTTP/1.1\r\nHost: nonexistent.com\r\n\r\n".as_bytes());
    let (_, _, live_cores_package) = relay1_mock.wait_for_package(&masquerader, Duration::from_secs(2)).unwrap();
    let (_, intended_exit_public_key) = CryptDENull::extract_key_pair(cheap_exit_key.len(), &live_cores_package.payload);
    assert_eq! (intended_exit_public_key, cheap_exit_key);
    let expired_cores_package: ExpiredCoresPackage<MessageType> = live_cores_package.to_expired(SocketAddr::from_str ("1.2.3.4:5678").unwrap(), &relay1_cryptde, &cheap_exit_cryptde).unwrap();

    // Respond with a DNS failure to put nonexistent.com on the unreachable-host list
    let client_request_vdata = match expired_cores_package.payload {
        MessageType::ClientRequest(vdata) => vdata,
        x => panic! ("Expected ClientRequest, got {:?}", x),
    };
    let stream_key = client_request_vdata
        .extract(&node_lib::sub_lib::migrations::client_request_payload::MIGRATIONS)
        .unwrap()
        .stream_key;
    let dns_fail_vdata = VersionedData::new (
        &node_lib::sub_lib::migrations::dns_resolve_failure::MIGRATIONS,
        &DnsResolveFailure_0v1 {
           stream_key,
        }
    );
    let dns_fail_pkg = IncipientCoresPackage::new (
        &originating_node_cryptde,
        Route::single_hop(originating_node.main_public_key(), &relay1_cryptde).unwrap(),
        MessageType::DnsResolveFailed(dns_fail_vdata),
        originating_node.main_public_key(),
    ).unwrap();
    relay1_mock.transmit_package(
        relay1_mock.port_list()[0],
        dns_fail_pkg,
        &masquerader,
        originating_node.main_public_key(),
        originating_node.socket_addr(PortSelector::First),
    ).unwrap();

    // This request should be routed through normal_exit because it's unreachable through cheap_exit
    client.send_chunk("GET / HTTP/1.1\r\nHost: nonexistent.com\r\n\r\n".as_bytes());
    let (_, _, live_cores_package) = relay1_mock.wait_for_package(&masquerader, Duration::from_secs(2)).unwrap();
    let (_, intended_exit_public_key) = CryptDENull::extract_key_pair(cheap_exit_key.len(), &live_cores_package.payload);
    assert_eq! (intended_exit_public_key, normal_exit_key);

    // Now request a different host; it should be routed through cheap_exit because it's cheaper
    client.send_chunk("GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".as_bytes());
    let (_, _, live_cores_package) = relay1_mock.wait_for_package(&masquerader, Duration::from_secs(2)).unwrap();
    let (_, intended_exit_public_key) = CryptDENull::extract_key_pair(cheap_exit_key.len(), &live_cores_package.payload);
    assert_eq! (intended_exit_public_key, cheap_exit_key);
}

fn cheaper_rate_pack (base_rate_pack: &RatePack, decrement: u64) -> RatePack {
    let mut result = *base_rate_pack;
    result.exit_byte_rate -= decrement;
    result.exit_service_rate -= decrement;
    result
}
