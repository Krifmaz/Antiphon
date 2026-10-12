//! Identity, session establishment, and per-message encryption.
//!
//! Scope relative to spec §3 "Connection & Trust Model" and §4
//! "Finding People":
//!
//! - **Identity is a keypair, never a name scoped to a server**
//!   ([`IdentityKeyPair`]). An Ed25519 key signs; a static X25519 prekey,
//!   signed by it, does Diffie-Hellman. Where to reach someone is a
//!   separate signed record ([`SignedRouteRecord`]) they can replace at
//!   will, so changing hosts never changes who they are.
//! - **The handshake authenticates** ([`HandshakeInit`]): an interactive
//!   triple-DH binding the session to both long-term identities, while
//!   keeping forward secrecy through a per-call ephemeral.
//! - **Humans can check the machine's work** ([`verification_code`]): a
//!   short code compared out of band, which is the only thing that
//!   detects a machine-in-the-middle holding a perfectly valid identity.
//!
//! - **Losing a device is survivable, losing everything is not silent**
//!   ([`mnemonic`], [`device`], [`backup`]). One master seed renders as a
//!   24-word recovery phrase; devices are cross-signed into a list by the
//!   master key; and history is sealed to those devices rather than to the
//!   phrase, so restoring an identity and restoring its messages are
//!   deliberately separate powers.
//! - **Contacts are identity keys with local petnames** ([`contact`]),
//!   pinned on first use, with a key change surfaced rather than resolved
//!   silently.
//!
//! Not yet implemented, each a larger piece of work building on
//! [`Session`]: the DH ratchet step (post-compromise security), MLS for
//! groups, and SFrame for relayed media.

pub mod backup;
mod contact;
mod device;
mod error;
mod handshake;
mod identity;
mod invite;
pub mod keystore;
pub mod mnemonic;
mod ratchet;
mod route;
mod session;
pub mod store;
mod verification;

pub use backup::{open_backup_key, seal_backup_key, BackupKey, SealedBackupKey};
pub use contact::{evaluate, fingerprint, PinnedContact, TrustDecision, TrustLevel};
pub use device::{DeviceCredential, DeviceKeyPair, DeviceList, SignedDeviceList};
pub use error::CryptoError;
pub use invite::Invite;
pub use handshake::{Handshake, HandshakeInit, HandshakeMessage};
pub use identity::{verify_signature, IdentityKeyPair, PublicIdentity};
pub use route::{Route, RouteRecord, SignedRouteRecord};
pub use session::{Sealed, Session};
pub use verification::verification_code;
