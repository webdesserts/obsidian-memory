/// Integration spike: validates that iroh and iroh-gossip work end-to-end
/// on native targets. This test is the feasibility check before the iroh
/// migration — it verifies gossip message delivery and NeighborUp events
/// between two peers on the same machine.
///
/// WASM: These tests are native-only (`#[cfg(feature = "native")]`) because
/// the iroh networking stack requires tokio and real sockets.
#[cfg(feature = "native")]
mod iroh_gossip_spike {
    use std::time::Duration;

    use futures::StreamExt;
    use iroh::{
        Endpoint, EndpointAddr, RelayMode,
        address_lookup::memory::MemoryLookup,
        endpoint::presets,
        protocol::Router,
    };
    use iroh_gossip::{
        Gossip, TopicId,
        api::Event,
        net::GOSSIP_ALPN,
    };

    fn topic_id(name: &str) -> TopicId {
        // TopicId is 32 bytes; derive one from a fixed seed via simple XOR
        // so that tests are deterministic and human-readable.
        let mut bytes = [0u8; 32];
        for (i, b) in name.as_bytes().iter().enumerate() {
            bytes[i % 32] ^= b;
        }
        TopicId::from_bytes(bytes)
    }

    /// Creates an iroh Endpoint with relay disabled. Both test peers are on
    /// the same machine, so they connect directly via their loopback addresses.
    /// A MemoryLookup is installed post-bind so each peer can resolve the
    /// other's direct socket address.
    async fn make_endpoint() -> anyhow::Result<(Endpoint, MemoryLookup)> {
        let ep = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await?;
        let memory_lookup = MemoryLookup::new();
        ep.address_lookup()?.add(memory_lookup.clone());
        Ok((ep, memory_lookup))
    }

    /// Two endpoints each create a Gossip instance, join the same TopicId,
    /// and exchange a message. Verifies:
    /// - A broadcast from one peer is received by the other
    #[tokio::test]
    async fn gossip_two_peers_can_exchange_messages() -> anyhow::Result<()> {
        let (ep_a, lookup_a) = make_endpoint().await?;
        let (ep_b, lookup_b) = make_endpoint().await?;

        // Share each endpoint's address with the other so they can dial each other directly.
        let addr_a: EndpointAddr = ep_a.addr();
        let addr_b: EndpointAddr = ep_b.addr();
        lookup_a.add_endpoint_info(addr_b.clone());
        lookup_b.add_endpoint_info(addr_a.clone());

        let gossip_a = Gossip::builder().spawn(ep_a.clone());
        let gossip_b = Gossip::builder().spawn(ep_b.clone());

        let _router_a = Router::builder(ep_a.clone())
            .accept(GOSSIP_ALPN, gossip_a.clone())
            .spawn();
        let _router_b = Router::builder(ep_b.clone())
            .accept(GOSSIP_ALPN, gossip_b.clone())
            .spawn();

        let topic = topic_id("iroh-spike-test-topic");

        // A joins first with no bootstrap peers.
        let (tx_a, mut rx_a) = gossip_a
            .subscribe(topic, vec![])
            .await?
            .split();
        let _ = tx_a; // A won't send in this test

        // B joins and bootstraps off A's endpoint ID so they find each other.
        let (tx_b, mut rx_b) = gossip_b
            .subscribe(topic, vec![ep_a.id()])
            .await?
            .split();

        // Wait for B to join the swarm (connected to at least one peer).
        tokio::time::timeout(Duration::from_secs(10), rx_b.joined())
            .await
            .expect("timed out waiting for B to join")
            .expect("B failed to join gossip topic");

        // B broadcasts a message.
        let payload: bytes::Bytes = b"hello from B".as_ref().into();
        tx_b.broadcast(payload.clone()).await?;

        // A should receive it. Drain events until we see a Received or timeout.
        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = rx_a
                    .next()
                    .await
                    .expect("rx_a stream ended before message arrived")?;
                match event {
                    Event::Received(msg) => break Ok::<_, anyhow::Error>(msg.content),
                    Event::NeighborUp(_) | Event::NeighborDown(_) | Event::Lagged => continue,
                }
            }
        })
        .await
        .expect("timed out waiting for A to receive message")?;

        assert_eq!(received, payload, "A received the wrong message content");

        Ok(())
    }

    /// Verifies that NeighborUp fires on both sides when two peers connect.
    #[tokio::test]
    async fn gossip_neighbor_up_fires_on_both_sides() -> anyhow::Result<()> {
        let (ep_a, lookup_a) = make_endpoint().await?;
        let (ep_b, lookup_b) = make_endpoint().await?;

        lookup_a.add_endpoint_info(ep_b.addr());
        lookup_b.add_endpoint_info(ep_a.addr());

        let gossip_a = Gossip::builder().spawn(ep_a.clone());
        let gossip_b = Gossip::builder().spawn(ep_b.clone());

        let _router_a = Router::builder(ep_a.clone())
            .accept(GOSSIP_ALPN, gossip_a.clone())
            .spawn();
        let _router_b = Router::builder(ep_b.clone())
            .accept(GOSSIP_ALPN, gossip_b.clone())
            .spawn();

        let topic = topic_id("iroh-spike-neighbor-up-topic");

        let (_tx_a, mut rx_a) = gossip_a
            .subscribe(topic, vec![])
            .await?
            .split();

        let (_tx_b, mut rx_b) = gossip_b
            .subscribe(topic, vec![ep_a.id()])
            .await?
            .split();

        // Wait for B's NeighborUp (B sees A come online).
        let neighbor_up_b = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = rx_b
                    .next()
                    .await
                    .expect("rx_b ended before NeighborUp")?;
                if let Event::NeighborUp(id) = event {
                    break Ok::<_, anyhow::Error>(id);
                }
            }
        })
        .await
        .expect("timed out waiting for B's NeighborUp")?;

        assert_eq!(
            neighbor_up_b,
            ep_a.id(),
            "B's NeighborUp should identify A"
        );

        // Wait for A's NeighborUp (A sees B come online).
        let neighbor_up_a = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = rx_a
                    .next()
                    .await
                    .expect("rx_a ended before NeighborUp")?;
                if let Event::NeighborUp(id) = event {
                    break Ok::<_, anyhow::Error>(id);
                }
            }
        })
        .await
        .expect("timed out waiting for A's NeighborUp")?;

        assert_eq!(
            neighbor_up_a,
            ep_b.id(),
            "A's NeighborUp should identify B"
        );

        Ok(())
    }
}
