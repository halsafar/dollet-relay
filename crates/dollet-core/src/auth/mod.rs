//! Authentication.
//!
//! Three credentials reach this project and all three land on a
//! [`crate::domain::User`]:
//!
//! - a JWT access token, issued by [`jwt`] against a username and password
//! - an API key, for clients that cannot do a login round trip
//! - a username and password directly, verified by [`password`] against the
//!   Django hash the importer carried over

pub mod jwt;
pub mod password;

use crate::domain::UserLevel;

pub use jwt::{Claims, Jwt, TokenPair, random_token};

/// Clients send the key in `X-API-Key`, or as `Authorization: ApiKey <key>`.
pub const API_KEY_HEADER: &str = "x-api-key";
pub const API_KEY_SCHEME: &str = "apikey";

/// API keys are compared by unique-index lookup rather than byte comparison.
/// That is not constant time, but the key is 256 bits of CSPRNG output, so the
/// only thing timing leaks is how far down a B-tree a miss got.
pub fn new_api_key() -> String {
    random_token(32)
}

/// Compare two secrets without leaking their common prefix through timing.
///
/// Lives here rather than at the call site because `dollet-server` has no
/// `subtle` dependency and should not grow one to compare two strings.
pub fn secret_eq(expected: &str, supplied: &str) -> bool {
    use subtle::ConstantTimeEq;
    // An empty expectation must never match, or a user with no key configured
    // would be reachable by sending nothing.
    !expected.is_empty() && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}

/// Levels are stored as integers so `>=` works in SQL. An unknown value rounds
/// *down* to the nearest known level: a row that says 5 must not become admin.
pub fn level_from_i64(value: i64) -> UserLevel {
    match value {
        v if v >= 10 => UserLevel::Admin,
        v if v >= 1 => UserLevel::Standard,
        _ => UserLevel::Streamer,
    }
}

pub fn level_value(level: UserLevel) -> i64 {
    level as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_levels_round_down() {
        assert_eq!(level_from_i64(-3), UserLevel::Streamer);
        assert_eq!(level_from_i64(0), UserLevel::Streamer);
        assert_eq!(level_from_i64(1), UserLevel::Standard);
        assert_eq!(level_from_i64(5), UserLevel::Standard);
        assert_eq!(level_from_i64(10), UserLevel::Admin);
        assert_eq!(level_from_i64(99), UserLevel::Admin);
    }

    #[test]
    fn levels_round_trip_through_the_database_representation() {
        for level in [UserLevel::Streamer, UserLevel::Standard, UserLevel::Admin] {
            assert_eq!(level_from_i64(level_value(level)), level);
        }
    }

    #[test]
    fn ordering_matches_privilege() {
        assert!(UserLevel::Admin > UserLevel::Standard);
        assert!(UserLevel::Standard > UserLevel::Streamer);
    }

    #[test]
    fn an_unset_secret_matches_nothing_not_even_the_empty_string() {
        assert!(!secret_eq("", ""));
        assert!(!secret_eq("", "anything"));
        assert!(secret_eq("fixturepass", "fixturepass"));
        assert!(!secret_eq("fixturepass", "fixturepas"));
        assert!(!secret_eq("fixturepass", "Fixturepass"));
    }

    #[test]
    fn api_keys_are_unguessable_and_unique() {
        let a = new_api_key();
        assert_ne!(a, new_api_key());
        assert!(a.len() >= 40);
    }
}
