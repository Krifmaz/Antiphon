//! Devices and cross-signing (spec §3 "Multi-device -- new devices added
//! by approving them from an existing device").
//!
//! The rule this module enforces: **an identity's devices are exactly the
//! ones an already-trusted device has signed into the list.** Holding the
//! recovery phrase makes you the identity, but it does not retroactively
//! make a new laptop a member of the device list -- that takes a signature
//! from the master key, which in practice means approving it from a device
//! you already have.
//!
//! Why that matters, and why it is not merely bureaucracy: the encrypted
//! backup key is sealed to devices in this list (see [`crate::backup`]).
//! So the list is what decides who can read history, and adding yourself
//! to it is the one action that cannot be done from nothing.
//!
//! Shape:
//!
//! - A device has its own keypair -- the same [`PublicIdentity`] shape as
//!   an account, reusing the same signed-prekey machinery. Devices talk
//!   and receive sealed data using their own keys, never the master.
//! - A [`DeviceCredential`] is that device's public key plus a signature
//!   from the **master identity** vouching for it. That signature is the
//!   cross-signing step.
//! - A [`SignedDeviceList`] is the full authorized set, signed and
//!   sequenced like a route record. Revoking a device means publishing a
//!   shorter list with a higher sequence.
//!
//! Revocation is by omission rather than by tombstone because the list is
//! the authority: a peer holding sequence 7 does not care what was in
//! sequence 6, only who is authorized now.

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair, PublicIdentity};

const CREDENTIAL_CONTEXT: &[u8] = b"voip-app/device/credential/v1";
const LIST_CONTEXT: &[u8] = b"voip-app/device/list/v1";

/// Upper bound on devices in one list, so a hostile list can't make a peer
/// allocate without limit.
const MAX_DEVICES: usize = 64;
/// Upper bound on a device's display name.
const MAX_NAME_LEN: usize = 64;

/// A device's own long-term keypair.
///
/// Structurally identical to an account identity -- same Ed25519 signing
/// key, same signed X25519 prekey -- because a device needs to do the same
/// two things: sign, and receive sealed data. Reusing the type means the
/// prekey-binding check is the same tested code path.
pub type DeviceKeyPair = IdentityKeyPair;

/// One authorized device: its public keys, a human label, and the master
/// identity's signature over both.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceCredential {
    pub device: PublicIdentity,
    /// What the user sees -- "Desktop", "Phone". Not security-relevant and
    /// not unique; the device key is the identifier.
    pub name: String,
    pub added_at_secs: u64,
    /// The master identity vouching for this device. This is the
    /// cross-signature.
    pub master_signature: [u8; 64],
}

impl DeviceCredential {
    /// Bytes the master signature covers.
    fn signing_bytes(device: &PublicIdentity, name: &str, added_at_secs: u64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(CREDENTIAL_CONTEXT);
        out.extend_from_slice(&device.encode());
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&added_at_secs.to_be_bytes());
        out
    }

    /// Approves a device, signing it into the identity.
    ///
    /// This is the operation that requires an existing trusted device:
    /// it needs the master signing key, which only an already-set-up
    /// device (or a freshly recovered phrase) holds.
    pub fn approve(
        master: &IdentityKeyPair,
        device: PublicIdentity,
        name: &str,
        added_at_secs: u64,
    ) -> Result<Self, CryptoError> {
        if name.len() > MAX_NAME_LEN {
            return Err(CryptoError::MalformedDeviceList);
        }
        // A device whose own prekey signature is broken must never be
        // signed into the list -- doing so would launder a malformed key
        // behind a valid master signature.
        device.verify()?;

        let signature = master.sign(&Self::signing_bytes(&device, name, added_at_secs));
        Ok(Self { device, name: name.to_string(), added_at_secs, master_signature: signature })
    }

    /// Checks this device really was approved by `identity_key`.
    pub fn verify(&self, identity_key: &[u8; 32]) -> Result<(), CryptoError> {
        self.device.verify()?;
        verify_signature(
            identity_key,
            &Self::signing_bytes(&self.device, &self.name, self.added_at_secs),
            &self.master_signature,
        )
    }
}

/// The set of devices currently authorized for an identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceList {
    pub identity_key: [u8; 32],
    /// Monotonic, same rollback protection as a route record: without it,
    /// a revoked device could replay an older list that still contains it.
    pub sequence: u64,
    pub devices: Vec<DeviceCredential>,
}

impl DeviceList {
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(LIST_CONTEXT);
        out.extend_from_slice(&self.identity_key);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&(self.devices.len() as u32).to_be_bytes());
        for device in &self.devices {
            out.extend_from_slice(&device.device.encode());
            out.extend_from_slice(&(device.name.len() as u32).to_be_bytes());
            out.extend_from_slice(device.name.as_bytes());
            out.extend_from_slice(&device.added_at_secs.to_be_bytes());
            out.extend_from_slice(&device.master_signature);
        }
        out
    }

    pub fn sign(self, master: &IdentityKeyPair) -> Result<SignedDeviceList, CryptoError> {
        if self.identity_key != master.public().identity_key {
            return Err(CryptoError::MalformedIdentity);
        }
        if self.devices.len() > MAX_DEVICES {
            return Err(CryptoError::MalformedDeviceList);
        }
        let signature = master.sign(&self.signing_bytes());
        Ok(SignedDeviceList { list: self, signature })
    }
}

/// A device list plus the master signature over it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignedDeviceList {
    pub list: DeviceList,
    pub signature: [u8; 64],
}

impl SignedDeviceList {
    /// Verifies the list signature *and* every credential inside it.
    ///
    /// Both halves are needed. The list signature proves the set is the
    /// one the identity published; each credential's signature proves the
    /// individual device was approved rather than spliced in.
    pub fn verify(&self) -> Result<(), CryptoError> {
        verify_signature(
            &self.list.identity_key,
            &self.list.signing_bytes(),
            &self.signature,
        )?;
        for device in &self.list.devices {
            device.verify(&self.list.identity_key)?;
        }
        Ok(())
    }

    /// Verifies, then checks this list supersedes what is already held.
    pub fn accept(&self, known_sequence: Option<u64>) -> Result<(), CryptoError> {
        self.verify()?;
        if let Some(known) = known_sequence {
            if self.list.sequence <= known {
                return Err(CryptoError::StaleDeviceList {
                    offered: self.list.sequence,
                    known,
                });
            }
        }
        Ok(())
    }

    /// Whether a given device key is currently authorized.
    ///
    /// The question [`crate::backup`] asks before sealing anything, and
    /// the question a peer asks before accepting a message from a device
    /// claiming to speak for this identity.
    pub fn authorizes(&self, device_key: &[u8; 32]) -> bool {
        self.list
            .devices
            .iter()
            .any(|d| &d.device.identity_key == device_key)
    }

    pub fn devices(&self) -> &[DeviceCredential] {
        &self.list.devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_of(
        master: &IdentityKeyPair,
        sequence: u64,
        devices: Vec<DeviceCredential>,
    ) -> SignedDeviceList {
        DeviceList { identity_key: master.public().identity_key, sequence, devices }
            .sign(master)
            .unwrap()
    }

    fn approve(master: &IdentityKeyPair, device: &DeviceKeyPair, name: &str) -> DeviceCredential {
        DeviceCredential::approve(master, device.public(), name, 1_700_000_000).unwrap()
    }

    #[test]
    fn an_approved_device_verifies_under_its_identity() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();

        let credential = approve(&master, &desktop, "Desktop");
        assert!(credential.verify(&master.public().identity_key).is_ok());
    }

    #[test]
    fn a_device_approved_by_someone_else_is_not_authorized_here() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        // Mallory can happily sign this device into *her* identity, but
        // that says nothing about ours.
        let credential = approve(&mallory, &device, "Phone");
        assert!(credential.verify(&master.public().identity_key).is_err());
    }

    #[test]
    fn a_signed_list_verifies_and_reports_its_members() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();

        let signed = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &phone, "Phone")],
        );

        assert!(signed.verify().is_ok());
        assert!(signed.authorizes(&desktop.public().identity_key));
        assert!(signed.authorizes(&phone.public().identity_key));
    }

    #[test]
    fn a_device_never_approved_is_not_authorized() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let stranger = DeviceKeyPair::generate();

        let signed = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]);

        // This is the property the whole module exists for: holding a
        // valid keypair is not membership. Someone must sign you in.
        assert!(!signed.authorizes(&stranger.public().identity_key));
    }

    #[test]
    fn splicing_an_unapproved_device_into_a_list_is_caught() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let intruder = DeviceKeyPair::generate();

        let mut signed = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]);
        // Mallory appends her own device, signed by herself.
        signed.list.devices.push(approve(&mallory, &intruder, "Intruder"));

        assert!(signed.verify().is_err());
    }

    #[test]
    fn revoking_a_device_is_publishing_a_shorter_list() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let lost_phone = DeviceKeyPair::generate();

        let before = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &lost_phone, "Phone")],
        );
        let after = list_of(&master, 2, vec![approve(&master, &desktop, "Desktop")]);

        assert!(before.authorizes(&lost_phone.public().identity_key));
        assert!(!after.authorizes(&lost_phone.public().identity_key));
        assert!(after.accept(Some(before.list.sequence)).is_ok());
    }

    #[test]
    fn a_revoked_device_cannot_replay_the_older_list_that_contained_it() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let lost_phone = DeviceKeyPair::generate();

        let before = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &lost_phone, "Phone")],
        );

        // The old list is perfectly signed, so only the sequence check
        // stops a stolen phone from re-authorizing itself.
        assert!(matches!(
            before.accept(Some(2)),
            Err(CryptoError::StaleDeviceList { offered: 1, known: 2 })
        ));
    }

    #[test]
    fn a_list_cannot_be_signed_for_another_identity() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();

        let list = DeviceList {
            identity_key: master.public().identity_key,
            sequence: 1,
            devices: vec![],
        };
        assert!(list.sign(&mallory).is_err());
    }

    #[test]
    fn tampering_with_a_device_name_breaks_the_signature() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();

        let mut credential = approve(&master, &desktop, "Desktop");
        credential.name = "Definitely Desktop".to_string();

        assert!(credential.verify(&master.public().identity_key).is_err());
    }

    #[test]
    fn a_device_with_a_broken_prekey_signature_is_never_approved() {
        let master = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        let mut public = device.public();
        public.prekey_signature[0] ^= 0x01;

        assert!(DeviceCredential::approve(&master, public, "Broken", 0).is_err());
    }

    #[test]
    fn an_empty_device_list_is_valid_and_authorizes_nobody() {
        let master = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        let signed = list_of(&master, 1, vec![]);

        assert!(signed.verify().is_ok());
        assert!(!signed.authorizes(&device.public().identity_key));
    }
}
