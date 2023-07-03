//! An implementation of the [IETF Messaging Layer Security](https://messaginglayersecurity.rocks)
//! end-to-end encryption (E2EE) protocol.
//!
//! ## What is MLS?
//!
//! MLS is a new IETF end-to-end encryption standard that is designed to
//! provide transport agnostic, asynchronous, and highly performant
//! communication between a group of clients.
//!
//! ## MLS Protocol Features
//!
//! * Multi-party E2EE [group evolution](https://messaginglayersecurity.rocks/mls-protocol/draft-ietf-mls-protocol.html#name-cryptographic-state-and-evo)
//! via a propose-then-commit mechanism.
//! * Asynchronous by design with pre-computed [key packages](https://messaginglayersecurity.rocks/mls-protocol/draft-ietf-mls-protocol.html#name-key-packages),
//! allowing members to be added to a group while offline.
//! * Customizable credential system with built in support for X.509 certificates.
//! * [Extension system](https://messaginglayersecurity.rocks/mls-protocol/draft-ietf-mls-protocol.html#name-extensions)
//! allowing for application specific data to be negotiated via the protocol.
//! * Strong forward secrecy and post compromise security.
//! * Crypto agility via support for multiple [ciphersuites](https://messaginglayersecurity.rocks/mls-protocol/draft-ietf-mls-protocol.html#name-mls-ciphersuites).
//! * Pre-shared key support.
//! * Subgroup branching.
//! * Group reinitialization (ex: protocol version upgrade).
//!
//! ## Crate Features
//!
//! * Easy to use client interface that manages multiple MLS identities and groups.
//! * 100% RFC conformance with support for all default credential, proposal,
//!   and extension types.
//! * Async API with async trait based extension points.
//! * Configurable storage for key packages, secrets and group state
//!   via provider traits along with default "in memory" implementations.
//! * Support for custom user created proposal, and extension types.
//! * Ability to create user defined credentials with custom validation
//!   routines that can bridge to existing credential schemes.
//! * OpenSSL and Rust Crypto based ciphersuite implementations.
//! * Crypto agility with support for user defined ciphersuites.
//! * High test coverage including security focused tests and
//!   pre-computed test vectors.
//! * Fuzz testing suite.
//! * Benchmarks for core functionality.
//!

#![allow(clippy::enum_variant_names)]
#![allow(clippy::result_large_err)]
#![allow(clippy::nonstandard_macro_braces)]
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

#[cfg(all(test, target_arch = "wasm32"))]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[cfg(test)]
macro_rules! hex {
    ($input:literal) => {
        hex::decode($input).expect("invalid hex value")
    };
}

#[cfg(test)]
macro_rules! load_test_case_json {
    ($name:ident, $generate:expr) => {
        load_test_case_json!($name, $generate, to_vec_pretty)
    };
    ($name:ident, $generate:expr, $to_json:ident) => {{
        #[cfg(any(target_arch = "wasm32", not(feature = "std")))]
        {
            // Do not remove `async`! (The goal of this line is to remove warnings
            // about `$generate` not being used. Actually calling it will make tests fail.)
            let _ = async { $generate };
            serde_json::from_slice(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/test_data/",
                stringify!($name),
                ".json"
            )))
            .unwrap()
        }

        #[cfg(all(not(target_arch = "wasm32"), feature = "std"))]
        {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/test_data/",
                stringify!($name),
                ".json"
            );
            if !std::path::Path::new(path).exists() {
                std::fs::write(path, serde_json::$to_json(&$generate).unwrap()).unwrap();
            }
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
        }
    }};
}

#[cfg(feature = "benchmark")]
macro_rules! load_test_case_mls {
    ($name:ident, $generate:expr) => {
        load_test_case_mls!($name, $generate, to_vec_pretty)
    };
    ($name:ident, $generate:expr, $to_json:ident) => {{
        #[cfg(any(target_arch = "wasm32", not(feature = "std")))]
        {
            // Do not remove `async`! (The goal of this line is to remove warnings
            // about `$generate` not being used. Actually calling it will make tests fail.)
            let _ = async { $generate };

            aws_mls_codec::MlsDecode::mls_decode(&mut &include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/test_data/",
                stringify!($name),
                ".mls"
            )))
            .unwrap()
        }

        #[cfg(all(not(target_arch = "wasm32"), feature = "std"))]
        {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/test_data/",
                stringify!($name),
                ".mls"
            );

            if !std::path::Path::new(path).exists() {
                std::fs::write(path, $generate.mls_encode_to_vec().unwrap()).unwrap();
            }

            aws_mls_codec::MlsDecode::mls_decode(&mut std::fs::read(path).unwrap().as_slice())
                .unwrap()
        }
    }};
}

mod cipher_suite {
    pub use aws_mls_core::crypto::CipherSuite;
}

pub use cipher_suite::CipherSuite;

mod protocol_version {
    pub use aws_mls_core::protocol_version::ProtocolVersion;
}

pub use protocol_version::ProtocolVersion;

mod client;
pub mod client_builder;
mod client_config;
/// Dependencies of [`CryptoProvider`] and [`CipherSuiteProvider`]
pub mod crypto;
/// Extension utilities and built-in extension types.
pub mod extension;
/// Tools to observe groups without being a member, useful
/// for server implementations.
#[cfg(feature = "external_client")]
pub mod external_client;
mod grease;
/// E2EE group created by a [`Client`].
pub mod group;
mod hash_reference;
/// Identity providers to use with [`ClientBuilder`](client_builder::ClientBuilder).
pub mod identity;
mod key_package;
/// Pre-shared key support.
pub mod psk;
mod signer;
/// Storage providers to use with
/// [`ClientBuilder`](client_builder::ClientBuilder).
pub mod storage_provider;

pub use aws_mls_core::{
    crypto::{CipherSuiteProvider, CryptoProvider},
    group::GroupStateStorage,
    identity::IdentityProvider,
    key_package::KeyPackageStorage,
    keychain::KeychainStorage,
    psk::PreSharedKeyStorage,
};

/// Dependencies of [`ProposalRules`].
pub mod proposal_rules {
    pub use crate::group::proposal_filter::{
        PassThroughProposalRules, ProposalBundle, ProposalInfo,
    };
}

pub use crate::group::proposal_filter::ProposalRules;

pub use aws_mls_core::extension::{Extension, ExtensionList};

pub use crate::client::Client;

pub use group::{
    framing::{MLSMessage, WireFormat},
    internal::Group,
};

/// Error types.
pub mod error {
    pub use crate::client::MlsError;
    pub use aws_mls_core::extension::ExtensionError;
}

/// WASM compatible timestamp.
pub mod time {
    pub use aws_mls_core::time::*;
}

#[cfg(feature = "benchmark")]
#[doc(hidden)]
pub mod bench_utils;

#[cfg(feature = "benchmark")]
#[doc(hidden)]
pub mod tree_kem;

#[cfg(not(feature = "benchmark"))]
mod tree_kem;

pub use aws_mls_codec;

mod private {
    pub trait Sealed {}
}

use private::Sealed;

#[cfg(any(test, feature = "test_utils"))]
#[doc(hidden)]
pub mod test_utils;
