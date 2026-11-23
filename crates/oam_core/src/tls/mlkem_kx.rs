//! The X25519MLKEM768 hybrid key-exchange group (IANA 0x11EC,
//! draft-ietf-tls-ecdhe-mlkem) on top of rustls's ring provider.
//!
//! Node v26's OpenSSL 3.5 offers this group first and sends a key share for
//! it and one for X25519, so two Node peers agree the hybrid and
//! `getEphemeralKeyInfo()` reports `{ type: 'TLSGroup', name:
//! 'X25519MLKEM768' }`. rustls implements the group only in its aws-lc-rs
//! provider, whose C build (aws-lc-sys) fails under MSVC on ARM64, so oam
//! builds it here from the two halves it can compile everywhere: RustCrypto's
//! pure-Rust FIPS 203 ML-KEM-768 (`ml-kem`) and ring's X25519 (rustls's own
//! `kx_group::X25519`). The wire layout is the draft's, and rustls's aws-lc
//! `Hybrid` with `post_quantum_first: true`:
//!
//! - client share: ML-KEM-768 encapsulation key (1184 bytes) || X25519 public
//!   key (32)
//! - server share: ML-KEM-768 ciphertext (1088) || X25519 public key (32)
//! - shared secret: ML-KEM shared secret (32) || X25519 shared secret (32)
//!
//! Like rustls's own hybrid, the client's active exchange exposes its X25519
//! half as a `hybrid_component`, so a rustls client offering
//! [X25519MLKEM768, X25519, ...] sends both key shares in one ClientHello --
//! OpenSSL 3.5's default pair -- and a server that has only X25519 picks that
//! share without a HelloRetryRequest. The group is TLS 1.3 only.
//!
//! Randomness is ring's `SystemRandom`: the decapsulation key comes from a
//! 64-byte seed (FIPS 203 `ML-KEM.KeyGen_internal(d, z)`) and an encapsulation
//! from a 32-byte message `m` (`ML-KEM.Encaps_internal`), both filled by it --
//! what `ml-kem`'s own rng entry points do, without bridging ring to
//! `rand_core`. The encapsulation-key check of FIPS 203 section 7.2 (the
//! modulus check) runs in `EncapsulationKey::new`; a share that fails it, or has
//! the wrong length, is rustls's `InvalidKeyShare`.

use ml_kem::array::Array;
use ml_kem::{B32, Decapsulate, DecapsulationKey768, EncapsulationKey768, KeyExport, Seed};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::crypto::{ActiveKeyExchange, CompletedKeyExchange, SharedSecret, SupportedKxGroup};
use rustls::{Error, NamedGroup, PeerMisbehaved, ProtocolVersion};
use zeroize::Zeroize;

/// ML-KEM-768 encapsulation key: the client's post-quantum share.
const MLKEM768_ENCAP_LEN: usize = 1184;
/// ML-KEM-768 ciphertext: the server's post-quantum share.
const MLKEM768_CIPHERTEXT_LEN: usize = 1088;
/// An X25519 public key.
const X25519_LEN: usize = 32;

const INVALID_KEY_SHARE: Error = Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare);

/// The X25519MLKEM768 group, for a provider's `kx_groups`.
pub static X25519MLKEM768: &dyn SupportedKxGroup = &X25519MlKem768;

#[derive(Debug)]
struct X25519MlKem768;

/// The classical half: ring's X25519, as rustls's ring provider builds it.
fn classical() -> &'static dyn SupportedKxGroup {
    rustls::crypto::ring::kx_group::X25519
}

fn fill_random(buf: &mut [u8]) -> Result<(), Error> {
    SystemRandom::new()
        .fill(buf)
        .map_err(|_| Error::FailedToGetRandomBytes)
}

/// `post_quantum || classical`, the draft's order for this group.
fn concat(post_quantum: &[u8], classical: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(post_quantum.len() + classical.len());
    out.extend_from_slice(post_quantum);
    out.extend_from_slice(classical);
    out
}

/// Split a received share into (ML-KEM part, X25519 part), refusing any
/// length but the exact one.
fn split(share: &[u8], post_quantum_len: usize) -> Result<(&[u8], &[u8]), Error> {
    if share.len() != post_quantum_len + X25519_LEN {
        return Err(INVALID_KEY_SHARE);
    }
    Ok(share.split_at(post_quantum_len))
}

impl SupportedKxGroup for X25519MlKem768 {
    /// Client: a fresh ML-KEM-768 key pair and a fresh X25519 key pair.
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let classical = classical().start()?;
        let mut seed = Seed::default();
        fill_random(&mut seed)?;
        let decaps_key = DecapsulationKey768::from_seed(seed);
        // The seed is the private key; from_seed copied it into the key,
        // whose drop zeroizes it ("zeroize" feature). Clear this copy too,
        // with a wipe the optimizer keeps (a fill(0) here is a dead store).
        seed.as_mut_slice().zeroize();
        let encaps_key = decaps_key.encapsulation_key().to_bytes();
        let pub_key = concat(&encaps_key, classical.pub_key());
        Ok(Box::new(Active {
            decaps_key: Box::new(decaps_key),
            classical,
            pub_key,
        }))
    }

    /// Server: encapsulate to the client's ML-KEM key and complete X25519
    /// against its X25519 key, in one step.
    fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
        let (pq_share, classical_share) = split(client_share, MLKEM768_ENCAP_LEN)?;
        let classical = classical().start_and_complete(classical_share)?;

        let encoded = Array::try_from(pq_share).map_err(|_| INVALID_KEY_SHARE)?;
        let encaps_key = EncapsulationKey768::new(&encoded).map_err(|_| INVALID_KEY_SHARE)?;
        let mut m = B32::default();
        fill_random(&mut m)?;
        let (ciphertext, mut pq_secret) = encaps_key.encapsulate_deterministic(&m);
        m.as_mut_slice().zeroize();

        let secret = SharedSecret::from(concat(&pq_secret, classical.secret.secret_bytes()));
        pq_secret.as_mut_slice().zeroize();
        Ok(CompletedKeyExchange {
            group: NamedGroup::X25519MLKEM768,
            pub_key: concat(&ciphertext, &classical.pub_key),
            secret,
        })
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }

    fn usable_for_version(&self, version: ProtocolVersion) -> bool {
        version == ProtocolVersion::TLSv1_3
    }
}

/// A client's half-finished exchange: the ML-KEM decapsulation key and the
/// X25519 exchange, waiting for the server's share.
struct Active {
    decaps_key: Box<DecapsulationKey768>,
    classical: Box<dyn ActiveKeyExchange>,
    pub_key: Vec<u8>,
}

impl ActiveKeyExchange for Active {
    /// The server's share is the ML-KEM ciphertext and its X25519 key.
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        let (pq_share, classical_share) = split(peer_pub_key, MLKEM768_CIPHERTEXT_LEN)?;
        let classical = self.classical.complete(classical_share)?;
        let ciphertext = Array::try_from(pq_share).map_err(|_| INVALID_KEY_SHARE)?;
        // ML-KEM decapsulation never fails: a ciphertext that does not
        // re-encrypt to itself yields the implicit-rejection key (FIPS 203),
        // and the handshake then fails at Finished, as with aws-lc.
        let mut pq_secret = self.decaps_key.decapsulate(&ciphertext);
        let secret = SharedSecret::from(concat(&pq_secret, classical.secret_bytes()));
        pq_secret.as_mut_slice().zeroize();
        Ok(secret)
    }

    /// The X25519 half, offered as its own key share for free.
    fn hybrid_component(&self) -> Option<(NamedGroup, &[u8])> {
        Some((self.classical.group(), self.classical.pub_key()))
    }

    /// The server picked the X25519 share: finish that half alone.
    fn complete_hybrid_component(
        self: Box<Self>,
        peer_pub_key: &[u8],
    ) -> Result<SharedSecret, Error> {
        self.classical.complete(peer_pub_key)
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519MLKEM768
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_have_the_drafts_lengths_and_both_ends_agree() {
        let client = X25519MLKEM768.start().unwrap();
        assert_eq!(client.group(), NamedGroup::X25519MLKEM768);
        assert_eq!(client.pub_key().len(), MLKEM768_ENCAP_LEN + X25519_LEN);
        // The X25519 component is the tail of the hybrid share.
        let (component_group, component) = client.hybrid_component().unwrap();
        assert_eq!(component_group, NamedGroup::X25519);
        assert_eq!(component, &client.pub_key()[MLKEM768_ENCAP_LEN..]);

        let server = X25519MLKEM768.start_and_complete(client.pub_key()).unwrap();
        assert_eq!(server.group, NamedGroup::X25519MLKEM768);
        assert_eq!(server.pub_key.len(), MLKEM768_CIPHERTEXT_LEN + X25519_LEN);
        assert_eq!(server.secret.secret_bytes().len(), 64);

        let client_secret = client.complete(&server.pub_key).unwrap();
        assert_eq!(client_secret.secret_bytes(), server.secret.secret_bytes());
    }

    #[test]
    fn the_component_path_is_plain_x25519() {
        // A server with only X25519 answers the component share; the client
        // finishes that half alone and agrees with it.
        let client = X25519MLKEM768.start().unwrap();
        let (_, x_share) = client.hybrid_component().unwrap();
        let x_server = classical().start_and_complete(x_share).unwrap();
        let secret = client.complete_hybrid_component(&x_server.pub_key).unwrap();
        assert_eq!(secret.secret_bytes(), x_server.secret.secret_bytes());
        assert_eq!(secret.secret_bytes().len(), 32);
    }

    #[test]
    fn wrong_lengths_and_bad_keys_are_invalid_key_shares() {
        let client = X25519MLKEM768.start().unwrap();
        let share = client.pub_key().to_vec();
        for bad in [&share[..share.len() - 1], &[][..], &[0u8; 1216 + 1][..]] {
            assert!(matches!(
                X25519MLKEM768.start_and_complete(bad),
                Err(e) if e == INVALID_KEY_SHARE
            ));
        }
        // An encapsulation key whose first coefficient is >= q (0xFFF) fails
        // FIPS 203's modulus check.
        let mut tampered = share.clone();
        tampered[0] = 0xff;
        tampered[1] |= 0x0f;
        assert!(matches!(
            X25519MLKEM768.start_and_complete(&tampered),
            Err(e) if e == INVALID_KEY_SHARE
        ));
        // A server share of the wrong length.
        assert!(matches!(
            client.complete(&[0u8; MLKEM768_CIPHERTEXT_LEN]),
            Err(e) if e == INVALID_KEY_SHARE
        ));
    }

    #[test]
    fn a_corrupted_ciphertext_yields_a_different_secret() {
        let client = X25519MLKEM768.start().unwrap();
        let mut server = X25519MLKEM768.start_and_complete(client.pub_key()).unwrap();
        server.pub_key[5] ^= 1;
        let secret = client.complete(&server.pub_key).unwrap();
        assert_ne!(secret.secret_bytes(), server.secret.secret_bytes());
    }

    #[test]
    fn tls13_only() {
        assert!(X25519MLKEM768.usable_for_version(ProtocolVersion::TLSv1_3));
        assert!(!X25519MLKEM768.usable_for_version(ProtocolVersion::TLSv1_2));
        assert_eq!(u16::from(X25519MLKEM768.name()), 0x11ec);
    }
}
