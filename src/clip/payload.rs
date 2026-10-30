//! The bytes of one representation.

use std::{fmt, ops::Deref, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

/// The bytes of one representation. Shared between the clipboard, the
/// backend serving them and the peers they are sent to, rather than copied
/// to each, and wiped when the last holder lets go: the clipboard is where
/// a copied password lives.
///
/// Encoded as a byte string, which postcard writes in one go rather than
/// byte by byte.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload(Arc<Zeroizing<Vec<u8>>>);

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Payload(Arc::new(Zeroizing::new(bytes)))
    }
}

impl Deref for Payload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Payload {
    /// Leaves the bytes out: they may be a password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Payload({} bytes)", self.len())
    }
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self)
    }
}

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(serde_bytes::ByteBuf::deserialize(deserializer)?
            .into_vec()
            .into())
    }
}
