use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use witnet_p2p::peers::*;

#[test]
fn p2p_peers_add_to_new() {
    // Create peers struct
    let mut peers = Peers::default();

    // Add address
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let src_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8080);

    assert_eq!(
        peers.add_to_new(vec![address], src_address).unwrap(),
        vec![]
    );
    // If we add the same address again, the method returns it
    assert_eq!(
        peers.add_to_new(vec![address], src_address).unwrap(),
        vec![address]
    );

    // Get a random address (there is only 1)
    let result = peers.get_random();

    // Check that both addresses are the same
    assert_eq!(result.unwrap(), Some(address));

    // There is only 1 address
    assert_eq!(peers.get_all_from_new().unwrap(), vec![address]);
}

#[test]
fn p2p_peers_add_to_tried() {
    // Create peers struct
    let mut peers = Peers::default();

    // Add address
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

    assert_eq!(peers.add_to_tried(address).unwrap(), None);
    // If we add the same address again, the method returns it
    assert_eq!(peers.add_to_tried(address).unwrap(), Some(address));

    // Get a random address (there is only 1)
    let result = peers.get_random().unwrap();

    // Check that both addresses are the same
    assert_eq!(result, Some(address));

    // There is only 1 address
    assert_eq!(peers.get_all_from_tried().unwrap(), vec![address]);
}

#[test]
fn p2p_peers_remove() {
    // Create peers struct
    let mut peers = Peers::default();

    // Add address
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    peers.add_to_tried(address).unwrap();

    // Remove address
    assert_eq!(peers.remove_from_tried(&[address]), vec![address]);

    // Get a random address
    let result = peers.get_random();

    // Check that both addresses are the same
    assert_eq!(result.unwrap(), None);

    // Remove the same address twice doesn't panic
    assert_eq!(peers.remove_from_tried(&[address, address]), vec![]);
}

#[test]
fn p2p_peers_get_all_from_new() {
    // Create peers struct
    let mut peers = Peers::default();

    // Add 100 addresses
    let many_peers: Vec<_> = (0..100)
        .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, i)), 8080))
        .collect();
    let src_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(168, 0, 0, 12)), 8080);
    peers.add_to_new(many_peers.clone(), src_address).unwrap();

    assert!(!peers.get_all_from_new().unwrap().is_empty());
    assert!(peers.get_all_from_tried().unwrap().is_empty());
}

#[test]
fn p2p_peers_get_all_from_tried() {
    // Create peers struct
    let mut peers = Peers::default();

    // Add 100 addresses
    for i in 0..100 {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, i)), 8080);
        peers.add_to_tried(address).unwrap();
    }

    assert!(peers.get_all_from_new().unwrap().is_empty());
    assert!(!peers.get_all_from_tried().unwrap().is_empty());
}
