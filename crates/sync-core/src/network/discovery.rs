//! Local network discovery via mDNS.
//!
//! Registers an mDNS-based address lookup with an iroh endpoint so that
//! peers on the same LAN can find each other without a relay or DNS.
//!
//! mDNS support requires enabling the `address-lookup-mdns` feature of the
//! `iroh` crate and adding `swarm-discovery` as a dependency. When not
//! compiled in, this module is a no-op.

use anyhow::Result;
use iroh::Endpoint;

/// Register mDNS-based local discovery with the endpoint.
///
/// After calling this, other iroh endpoints on the same LAN that also have
/// mDNS enabled will be able to dial this node by `EndpointId` alone,
/// without needing a relay URL or pre-known direct address.
///
/// Currently a no-op. To enable mDNS, add `address-lookup-mdns` to the
/// `iroh` feature flags in `Cargo.toml` and enable the `iroh/address-lookup-mdns`
/// feature in the `native` feature set, then uncomment the implementation below.
pub fn enable_mdns(_endpoint: &Endpoint) -> Result<()> {
    // To enable mDNS:
    //
    // 1. In Cargo.toml, add to the `native` feature:
    //    native = [..., "iroh/address-lookup-mdns"]
    //
    // 2. Uncomment:
    //    use iroh::address_lookup::MdnsAddressLookup;
    //    let mdns = MdnsAddressLookup::builder()
    //        .build(_endpoint.id())
    //        .map_err(|e| anyhow::anyhow!("Failed to build mDNS lookup: {e}"))?;
    //    _endpoint
    //        .address_lookup()
    //        .map_err(|e| anyhow::anyhow!("Failed to get address lookup: {e}"))?
    //        .add(mdns);

    Ok(())
}
