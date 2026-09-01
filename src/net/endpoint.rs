//! Binding the iroh endpoint that carries every peer connection.

use color_eyre::eyre::Result;
use iroh::{Endpoint, endpoint::presets};

use crate::config::MachineKey;

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
) -> Result<Endpoint> {
    match options {
        EndpointOptions::Production => Ok(Endpoint::builder(presets::N0)
            .secret_key(key.secret().clone())
            .alpns(alpns)
            .bind()
            .await?),
        #[cfg(feature = "test-util")]
        EndpointOptions::LocalTest { lookup } => {
            let endpoint = Endpoint::builder(presets::Minimal)
                .relay_mode(iroh::RelayMode::Disabled)
                .secret_key(key.secret().clone())
                .alpns(alpns)
                .bind()
                .await?;
            endpoint
                .address_lookup()
                .expect("the endpoint was just bound")
                .add(lookup.clone());
            lookup.add_endpoint_info(endpoint.addr());

            Ok(endpoint)
        }
    }
}
