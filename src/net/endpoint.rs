//! Binding the iroh endpoint that carries every peer connection.

use iroh::{
    Endpoint,
    endpoint::{QuicTransportConfig, presets},
};

use crate::config::MachineKey;

/// Unidirectional streams a peer may have open at once. Past it, the peer
/// waits for one to close rather than having its message dropped.
const MAX_UNI_STREAMS: u32 = 16;

/// Bidirectional streams a peer may have open at once.
const MAX_BI_STREAMS: u32 = 4;

/// How the endpoint reaches the network.
#[derive(Debug, Clone, Default)]
pub enum EndpointOptions {
    /// The iroh defaults: public relays for the handshake, then a direct
    /// hole-punched path whenever the network allows one.
    #[default]
    Production,
    /// Hermetic mode for tests: no relays, no external discovery. Every
    /// endpoint bound with the same `lookup` registers its address there
    /// and resolves the others through it, so they dial each other by id
    /// without leaving the machine.
    #[cfg(feature = "test-util")]
    LocalTest {
        lookup: iroh::address_lookup::MemoryLookup,
    },
}

impl EndpointOptions {
    /// Whether endpoints bound with these options use iroh's relays. The
    /// pairing host waits for a relay address before printing a ticket,
    /// and there is nothing to wait for without them.
    pub fn uses_relays(&self) -> bool {
        match self {
            EndpointOptions::Production => true,
            #[cfg(feature = "test-util")]
            EndpointOptions::LocalTest { .. } => false,
        }
    }
}

/// Binds the endpoint under this machine's identity. `alpns` lists the
/// protocols accepted from incoming connections.
pub async fn bind_endpoint(
    key: &MachineKey,
    alpns: Vec<Vec<u8>>,
    options: &EndpointOptions,
) -> eyre::Result<Endpoint> {
    match options {
        EndpointOptions::Production => Ok(Endpoint::builder(presets::N0)
            .transport_config(transport())
            .secret_key(key.secret().clone())
            .alpns(alpns)
            .bind()
            .await?),
        #[cfg(feature = "test-util")]
        EndpointOptions::LocalTest { lookup } => {
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
    }
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
