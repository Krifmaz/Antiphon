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
//! - **Sessions are a Double Ratchet** ([`Session`]): symmetric chains for
//!   forward secrecy, plus DH steps every few seconds of traffic so a
//!   stolen chain key stops working (post-compromise security).
//!
//! - **Groups are MLS** ([`group`]): RFC 9420 through `openmls`, with
//!   every member's credential being their identity key, so a group member
//!   is the same verified person as a 1:1 contact.
//! - **Communities are signed manifests** ([`community`]): versioned,
//!   each version signed by someone allowed to make it, so roles, bans and
//!   membership are enforced by every client with no server to ask.
//! - **Media a relay forwards is SFrame** ([`sframe`]): RFC 9605, keyed
//!   per epoch and per sender from the MLS exporter, so a forwarder never
//!   holds a key and someone who left can't hear what follows.
//! - **Push wake-ups name nobody** ([`push`]): sealed to the woken
//!   device's own push key, constant size, so a push service relays a
//!   knock without learning who knocked or why.
//!
//! # Frozen labels
//!
//! The domain-separation labels in this crate (`b"voip-app/..."`), the
//! keystore's `VOIPKEY1` marker and its sealing entropy still carry the
//! project's name from before it was called Antiphon, and they must stay
//! that way. They are inputs to key derivation and to on-disk formats, not
//! names: change the identity labels and every recovery phrase restores a
//! *different* identity, tripping a key-change warning with every contact;
//! change the verification or handshake labels and two builds can no longer
//! talk or agree on a code; change the keystore constants and every sealed
//! key on disk becomes unreadable. A rename touches what people see. These
//! are not that.

pub mod backup;
mod contact;
mod device;
pub mod dht_record;
mod error;
pub mod community;
pub mod group;
mod handshake;
mod identity;
mod invite;
pub mod keystore;
pub mod file_seal;
pub mod mail;
pub mod memory_seal;
pub mod mnemonic;
pub mod onion;
pub mod persona;
pub mod push;
mod ratchet;
pub mod relay_token;
pub mod shamir;
mod route;
mod session;
pub mod sframe;
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
