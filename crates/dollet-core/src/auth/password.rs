//! Django's `pbkdf2_sha256$<iterations>$<salt>$<b64hash>` password format.
//!
//! The importer carries hashes across verbatim; failing to verify them would
//! mean every user resets their password on cutover. New passwords are written
//! in the same format so there is one verification path rather than two, and
//! so the database stays readable by the instance it came from.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use pbkdf2::pbkdf2_hmac;
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

const ALGORITHM: &str = "pbkdf2_sha256";

/// Django 5.2's default, and what imported instances carry. Only
/// applies to newly written hashes; existing ones carry their own count.
const DEFAULT_ITERATIONS: u32 = 1_200_000;

/// Django's salt length: 22 characters from `[A-Za-z0-9]`.
const SALT_LEN: usize = 22;

/// Refuse to spend unbounded CPU on a hash claiming an absurd work factor. A
/// corrupted row should fail the login, not wedge a request thread.
const MAX_ITERATIONS: u32 = 10_000_000;

/// A real hash of a value nobody can supply, for verifying against when the
/// account does not exist.
///
/// A *constant*, not a freshly generated one: generating a hash costs a full
/// PBKDF2 run on top of the verify, so the "constant time" path would answer a
/// missing user in twice the time of a wrong password — an enumeration oracle
/// built by the code written to close one.
pub const UNUSABLE: &str =
    "pbkdf2_sha256$1200000$IpxUnusablePasswordAA$bJSwSL0EGD42TmWSDdxsQ6xCPNzS1IYgOFKe+nBQqbI=";

struct Parts<'a> {
    iterations: u32,
    salt: &'a str,
    digest: Vec<u8>,
}

/// How many key derivations this process has run.
///
/// One relaxed increment next to 1.2 million HMAC rounds costs nothing, and it
/// turns "a login costs the same whether or not the account exists" from a
/// wall-clock measurement — hopelessly noisy in an unoptimised build — into an
/// exact assertion.
static DERIVATIONS: AtomicU64 = AtomicU64::new(0);

pub fn derivations() -> u64 {
    DERIVATIONS.load(Ordering::Relaxed)
}

fn derive(password: &str, salt: &str, iterations: u32, out: &mut [u8]) {
    DERIVATIONS.fetch_add(1, Ordering::Relaxed);
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), iterations, out);
}

fn parse(encoded: &str) -> Result<Parts<'_>> {
    let mut fields = encoded.splitn(4, '$');
    let (Some(algorithm), Some(iterations), Some(salt), Some(digest)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(Error::invalid("password hash is not in four `$` parts"));
    };

    if algorithm != ALGORITHM {
        return Err(Error::invalid(format!(
            "unsupported password algorithm `{algorithm}`"
        )));
    }

    let iterations: u32 = iterations
        .parse()
        .map_err(|_| Error::invalid("password hash iteration count is not a number"))?;
    if iterations == 0 || iterations > MAX_ITERATIONS {
        return Err(Error::invalid("implausible password hash iteration count"));
    }

    let digest = BASE64
        .decode(digest)
        .map_err(|_| Error::invalid("password hash digest is not base64"))?;
    if digest.is_empty() {
        return Err(Error::invalid("password hash digest is empty"));
    }

    Ok(Parts {
        iterations,
        salt,
        digest,
    })
}

pub fn hash(password: &str) -> String {
    hash_with(password, DEFAULT_ITERATIONS)
}

fn hash_with(password: &str, iterations: u32) -> String {
    let salt: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(SALT_LEN)
        .map(char::from)
        .collect();

    let mut derived = [0u8; 32];
    pbkdf2_hmac::<Sha256>(
        password.as_bytes(),
        salt.as_bytes(),
        iterations,
        &mut derived,
    );

    format!("{ALGORITHM}${iterations}${salt}${}", BASE64.encode(derived))
}

/// Verify on a blocking thread.
///
/// 1.2M PBKDF2 iterations is tens of milliseconds of solid CPU with no await
/// points. Run inline it occupies a tokio worker — the same workers pumping
/// bytes to Plex — so a handful of concurrent unauthenticated logins is a
/// denial of service against the streaming path.
pub async fn verify_offthread(password: &str, encoded: &str) -> Result<bool> {
    let password = password.to_owned();
    let encoded = encoded.to_owned();
    tokio::task::spawn_blocking(move || verify(&password, &encoded))
        .await
        .map_err(|e| Error::Other(e.into()))?
}

/// Hash on a blocking thread, for the same reason as [`verify_offthread`].
pub async fn hash_offthread(password: &str) -> String {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hash(&password))
        .await
        .unwrap_or_else(|_| hash(&password_placeholder()))
}

/// Only reachable if the blocking pool is shutting down, in which case any
/// value works: the caller is about to fail anyway, and this one can never be
/// matched by a real password.
fn password_placeholder() -> String {
    UNUSABLE.to_owned()
}

pub fn verify(password: &str, encoded: &str) -> Result<bool> {
    let parts = parse(encoded)?;

    // Derive to the stored length rather than a fixed 32: Django permits a
    // non-default `dklen`, and a length mismatch must read as "wrong password"
    // rather than panic on a slice of the wrong size.
    let mut derived = vec![0u8; parts.digest.len()];
    derive(password, parts.salt, parts.iterations, &mut derived);

    Ok(bool::from(derived.ct_eq(&parts.digest)))
}

/// Whether a stored hash could ever be verified, without knowing the password.
/// The importer reports unusable credentials with this rather than letting the
/// user discover them at the login screen.
pub fn is_supported(encoded: &str) -> bool {
    parse(encoded).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by Python's `hashlib.pbkdf2_hmac`, which is exactly what
    /// Django's `PBKDF2PasswordHasher` calls. A fixed vector is the only thing
    /// that proves this implementation is byte-compatible with the hashes the
    /// importer carries over, rather than merely self-consistent.
    const DJANGO_HASH: &str =
        "pbkdf2_sha256$1000$saltysalt$TGvLvWQ3FS0hm6LiY2GRRjIIFTC/Q5rp07HyYd7TgIA=";

    #[test]
    fn verifies_a_hash_produced_by_django() {
        assert!(verify("correct horse battery staple", DJANGO_HASH).unwrap());
        assert!(!verify("wrong horse battery staple", DJANGO_HASH).unwrap());
        assert!(!verify("", DJANGO_HASH).unwrap());
    }

    #[test]
    fn round_trips_its_own_hashes() {
        // Deliberately not `hash()`: 1.2M iterations in an unoptimised test
        // build is seconds per call, and the round trip proves the same thing.
        let encoded = hash_with("hunter2", 1_000);
        assert!(verify("hunter2", &encoded).unwrap());
        assert!(!verify("hunter3", &encoded).unwrap());
    }

    #[test]
    fn writes_the_django_header_and_a_fresh_salt() {
        let a = hash_with("same", 1_000);
        let b = hash_with("same", 1_000);
        assert_ne!(a, b, "salt was reused");

        let fields: Vec<&str> = a.split('$').collect();
        assert_eq!(fields[0], ALGORITHM);
        assert_eq!(fields[2].len(), SALT_LEN);
        assert!(fields[2].chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(BASE64.decode(fields[3]).unwrap().len(), 32);
    }

    #[test]
    fn the_shipped_iteration_count_is_djangos() {
        assert!(hash("x").starts_with("pbkdf2_sha256$1200000$"));
    }

    #[test]
    fn rejects_hashes_it_cannot_check() {
        for bad in [
            "",
            "plaintext",
            "argon2$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA",
            "pbkdf2_sha256$notanumber$salt$aGFzaA==",
            "pbkdf2_sha256$0$salt$aGFzaA==",
            "pbkdf2_sha256$99999999$salt$aGFzaA==",
            "pbkdf2_sha256$1000$salt$not base64!",
            "pbkdf2_sha256$1000$salt$",
        ] {
            assert!(verify("x", bad).is_err(), "{bad} was accepted");
            assert!(!is_supported(bad), "{bad} reported supported");
        }
    }

    #[test]
    fn recognises_a_usable_hash_without_the_password() {
        assert!(is_supported(DJANGO_HASH));
    }

    #[test]
    fn the_unusable_hash_is_well_formed_and_matches_nothing_guessable() {
        assert!(is_supported(UNUSABLE));
        // One attempt only: at shipped strength each verify is seconds in an
        // unoptimised test build, and one is enough to prove it is a real hash
        // of something no request can carry rather than a sentinel string.
        assert!(!verify("", UNUSABLE).unwrap());
    }

    #[tokio::test]
    async fn the_offthread_wrappers_agree_with_the_inline_ones() {
        // Against the 1,000-iteration vector, so the test costs milliseconds
        // rather than the seconds a shipped-strength hash takes unoptimised.
        assert!(
            verify_offthread("correct horse battery staple", DJANGO_HASH)
                .await
                .unwrap()
        );
        assert!(!verify_offthread("wrong", DJANGO_HASH).await.unwrap());
        assert!(verify_offthread("x", "nonsense").await.is_err());
    }
}
