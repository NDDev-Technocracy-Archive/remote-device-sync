#![cfg(unix)]
// The included shared fixture consumes these explicit test-target bindings.
use iroh::{EndpointId as BackendEndpointId, SecretKey as BackendSecretKey};
use rcgen::{CertifiedKey, KeyPair, generate_simple_self_signed};
#[cfg(feature = "owned-relay")]
use rds_net::EndpointAddr;
const BINARY: &str = env!("CARGO_BIN_EXE_rds-relay");
const BIND_FLAG: &str = "--addr";
const HOSTS_DIRECTORY: bool = false;
include!("../../../tests/support/relay_runtime.rs");
