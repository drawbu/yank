//! This machine's identity on the mesh (`identity.key`).
//!
//! The private half is what iroh authenticates connections with, so it
//! must never be shared between machines; the public half is the endpoint
//! id peers dial. Stored base64-encoded, owner-readable only.

use std::{
    fs,
    io::{self, Write as _},
    path::Path,
};

use color_eyre::eyre::{Result, WrapErr as _, eyre};
use data_encoding::BASE64;
use iroh::{EndpointId, SecretKey};

use super::Dirs;

/// The machine's secret identity key.
#[derive(Clone, Debug)]
pub struct MachineKey(SecretKey);

impl MachineKey {
    /// Loads the key, generating and persisting one on first use.
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let path = dirs.identity_file();

        match fs::read_to_string(&path) {
            Ok(content) => Self::decode(&content),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Self::generate(&path),
            Err(err) => Err(err).wrap_err_with(|| format!("cannot read {}", path.display())),
        }
    }

    /// This machine's public identity, which peers dial.
    pub fn endpoint_id(&self) -> EndpointId {
        self.0.public()
    }

    /// The secret half, needed to bind the iroh endpoint.
    pub fn secret(&self) -> &SecretKey {
        &self.0
    }

    fn decode(content: &str) -> Result<Self> {
        let key: [u8; 32] = BASE64
            .decode(content.trim_ascii().as_bytes())
            .wrap_err("cannot decode the identity key")?
            .try_into()
            .map_err(|_| eyre!("cannot decode the identity key: expected 32 bytes"))?;

        Ok(Self(SecretKey::from_bytes(&key)))
    }

    /// Writes a fresh key, owner-only and refusing to overwrite: losing an
    /// identity means every peer has to pair again.
    fn generate(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;

        let key = SecretKey::generate();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(BASE64.encode(&key.to_bytes()).as_bytes()))
            .wrap_err_with(|| format!("cannot write {}", path.display()))?;

        Ok(Self(key))
    }
}
