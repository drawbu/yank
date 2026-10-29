//! Binding the iroh endpoint that carries every peer connection.

use iroh::{
    Endpoint,
    address_lookup::MemoryLookup,
    endpoint::{QuicTransportConfig, presets},
};

use crate::config::MachineKey;

/// Unidirectional streams a peer may have open at once. Past it, the peer
/// waits for one to close rather than having its message dropped.
const MAX_UNI_STREAMS: u32 = 16;

/// Bidirectional streams a peer may have open at once.
const MAX_BI_STREAMS: u32 = 4;

/// Binds the endpoint under this machine's identity. `alpns` lists the
/// protocols accepted from incoming connections.
///
/// With no `lookup`, the endpoint uses the iroh defaults: public relays for
/// the handshake, then a direct hole-punched path whenever the network
/// allows one. With one, it is hermetic, for tests: no relays, no external
/// discovery. Every endpoint bound with the same `lookup` registers its
/// address there and resolves the others through it, so they dial each
/// other by id without leaving the machine.
pub async fn bind_endpoint(
    key: &MachineKey,
    alpns: Vec<Vec<u8>>,
    lookup: Option<&MemoryLookup>,
) -> eyre::Result<Endpoint> {
    let Some(lookup) = lookup else {
        return Ok(Endpoint::builder(presets::N0)
            .transport_config(transport())
            .secret_key(key.secret().clone())
            .alpns(alpns)
            .bind()
            .await?);
    };

    let endpoint = Endpoint::builder(presets::Minimal)
        .transport_config(transport())
        .address_lookup(lookup.clone())
        .secret_key(key.secret().clone())
        .alpns(alpns)
        .bind()
        .await?;
    lookup.add_endpoint_info(endpoint.addr());

    Ok(endpoint)
}

/// The iroh defaults, with bounds on the streams a peer may open: it is
/// authenticated, which is not a licence to make us spawn as much work as
/// it likes. Set here so the handshake advertises them; lowering them on a
/// connection only takes effect once the peer's first allowance is spent.
fn transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_concurrent_uni_streams(MAX_UNI_STREAMS.into())
        .max_concurrent_bidi_streams(MAX_BI_STREAMS.into())
        .build()
}
