//! An account's mail key: what offline mail is sealed to, and where it is
//! kept, separately from the identity itself.
//!
//! Every device of an account holds the identity's master seed, so the
//! identity's own prekey and the inbox labels derived from the seed can't
//! be taken back from a device once it has them. The mail key can. It is a
//! random secret, made fresh whenever a device is removed from the account
//! and handed only to the devices still on it (sealed to each device's own
//! key). From then on:
//!
//! - contacts seal offline mail to the mail key's prekey, which the
//!   identity vouches for with a [`MailKeyCert`] published in its route
//!   record, and
//! - the inbox labels and fetch secrets at every node are derived from the
//!   mail key ([`crate::mail::inbox_label`] on [`holder`]), so the removed
//!   device can neither find the new inboxes nor open what is in them.
//!
//! Epoch 0 is the identity itself -- an account that has never removed a
//! device has no separate mail key, and its mail works exactly as before.

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair};

/// The route record entry kind a [`MailKeyCert`] travels under.
pub const ROUTE_KIND: &str = "mail-key";
/// Domain separation for the certificate signature.
const CERT_CONTEXT: &[u8] = b"stoop/mail-key/cert/v1";
/// Encoded certificate length: epoch, prekey, signature.
const CERT_LEN: usize = 8 + 32 + 64;

/// The keypair a mail key's secret stands for: its prekey seals and opens
/// mail, and its seed is what inbox labels and fetch secrets come from.
/// Built the way an identity is, so [`crate::mail`] works on it unchanged.
pub fn holder(secret: [u8; 32]) -> IdentityKeyPair {
    IdentityKeyPair::from_master_seed(secret)
}

/// The identity vouching that mail for it should be sealed to this prekey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailKeyCert {
    /// Which mail key: higher is newer.
    pub epoch: u64,
    /// The X25519 prekey to seal to.
    pub prekey: [u8; 32],
    /// The identity's signature over the epoch and prekey.
    pub signature: [u8; 64],
}

impl MailKeyCert {
    /// The identity (`account`) vouches for `holder`'s prekey as mail key
    /// number `epoch`.
    pub fn issue(account: &IdentityKeyPair, epoch: u64, holder: &IdentityKeyPair) -> Self {
        let prekey = holder.public().prekey;
        let signature = account.sign(&signing_bytes(&account.public().identity_key, epoch, &prekey));
        Self { epoch, prekey, signature }
    }

    /// Checks the identity `identity_key` really issued this.
    pub fn verify(&self, identity_key: &[u8; 32]) -> Result<(), CryptoError> {
        verify_signature(identity_key, &signing_bytes(identity_key, self.epoch, &self.prekey), &self.signature)
    }

    /// The certificate as a route address: hex of epoch (u64 BE), prekey,
    /// signature -- 208 characters, inside a route's 256.
    pub fn to_address(&self) -> String {
        let mut bytes = Vec::with_capacity(CERT_LEN);
        bytes.extend_from_slice(&self.epoch.to_be_bytes());
        bytes.extend_from_slice(&self.prekey);
        bytes.extend_from_slice(&self.signature);
        crate::hex::encode(&bytes)
    }

    /// Reads [`MailKeyCert::to_address`]. Unverified: call
    /// [`MailKeyCert::verify`] before sealing to it.
    pub fn from_address(address: &str) -> Option<Self> {
        let bytes: [u8; CERT_LEN] = crate::hex::decode_array(address)?;
        Some(Self {
            epoch: u64::from_be_bytes(bytes[..8].try_into().ok()?),
            prekey: bytes[8..40].try_into().ok()?,
            signature: bytes[40..].try_into().ok()?,
        })
    }
}

/// What the certificate signature covers. The identity key is included so
/// a certificate can't be lifted from one identity's record to another's.
fn signing_bytes(identity_key: &[u8; 32], epoch: u64, prekey: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CERT_CONTEXT.len() + 32 + 8 + 32);
    out.extend_from_slice(CERT_CONTEXT);
    out.extend_from_slice(identity_key);
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(prekey);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail;

    #[test]
    fn mail_sealed_to_the_mail_key_opens_only_with_it() {
        let account = IdentityKeyPair::generate();
        let sender = IdentityKeyPair::generate();
        let key = holder([7; 32]);
        let cert = MailKeyCert::issue(&account, 1, &key);
        assert!(cert.verify(&account.public().identity_key).is_ok());

        let me = account.public().identity_key;
        let sealed = mail::seal(&sender, &me, &cert.prekey, b"hi").unwrap();
        let (from, body) = mail::open_for(&me, &key, &sealed).unwrap();
        assert_eq!((from.identity_key, body.as_slice()), (sender.public().identity_key, &b"hi"[..]));
        // The identity's own prekey -- which every device ever added holds --
        // does not open it.
        assert!(mail::open(&account, &sealed).is_err());
        // Nor does an older mail key.
        assert!(mail::open_for(&me, &holder([6; 32]), &sealed).is_err());
    }

    #[test]
    fn a_new_mail_key_moves_the_inboxes() {
        let (old, new) = (holder([1; 32]), holder([2; 32]));
        assert_ne!(mail::inbox_label(&old, "node:3479"), mail::inbox_label(&new, "node:3479"));
    }

    #[test]
    fn the_certificate_survives_a_route_and_binds_its_identity() {
        let account = IdentityKeyPair::generate();
        let cert = MailKeyCert::issue(&account, 9, &holder([3; 32]));
        let address = cert.to_address();
        assert!(address.len() <= 256);
        let back = MailKeyCert::from_address(&address).unwrap();
        assert_eq!(back, cert);
        assert!(back.verify(&IdentityKeyPair::generate().public().identity_key).is_err());
        let mut later = back;
        later.epoch = 10;
        assert!(later.verify(&account.public().identity_key).is_err());
        assert!(MailKeyCert::from_address("abc").is_none());
    }
}
