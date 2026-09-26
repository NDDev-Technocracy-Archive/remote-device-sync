#![cfg(unix)]
const BINARY: &str = env!("CARGO_BIN_EXE_rds-server");
const BIND_FLAG: &str = "--relay-addr";
const HOSTS_DIRECTORY: bool = true;
include!("../../../tests/support/relay_runtime.rs");

#[tokio::test]
async fn invalid_host_inputs_are_rejected_before_identity_or_catalog_creation() {
    for case in 0..10 {
        let scratch = Scratch::new();
        let authority = rds_discovery::EndpointKey(
            ed25519_dalek::SigningKey::from_bytes(&[157; 32])
                .verifying_key()
                .to_bytes(),
        )
        .to_string();
        let missing = scratch.0.join("missing");
        let registry = scratch.0.join("registry.json");
        fs::write(&registry, b"{ malformed JSON").unwrap();
        let (cert, key) = tls_fixture(&scratch);
        let mut values = match case {
            0 => args(&["--registry-key", "!invalid!"]),
            1 => args(&["--registry-key", &authority, "--registry-epoch", "0"]),
            2 => args(&["--registry-epoch", "1"]),
            3 => args(&["--registry", registry.to_str().unwrap()]),
            4 => args(&[
                "--registry-key",
                &authority,
                "--registry",
                registry.to_str().unwrap(),
            ]),
            5 => args(&[
                "--registry-key",
                &authority,
                "--authority-rotation",
                missing.to_str().unwrap(),
            ]),
            6 => args(&["--directory-allow", "not-a-key"]),
            _ => {
                match case {
                    7 => fs::write(&cert, b"bad certificate").unwrap(),
                    8 => fs::write(&key, rcgen::KeyPair::generate().unwrap().serialize_pem())
                        .unwrap(),
                    _ => fs::write(&key, vec![b'x'; 64 * 1024 + 1]).unwrap(),
                }
                args(&[
                    "--directory-tls-cert",
                    cert.to_str().unwrap(),
                    "--directory-tls-key",
                    key.to_str().unwrap(),
                ])
            }
        };
        // Valid owned-relay flags ensure a host input error is caught before
        // the persistent relay identity is initialized as well as the catalog.
        // Probe the compiled relay library — feature unification can put the
        // owned backend into the spawned binary while this package's flag is
        // off.
        if rds_relay::OWNED_BACKEND_COMPILED {
            values.extend(args(&[
                "--relay-backend",
                "noq",
                "--relay-key-file",
                &scratch.key(),
                "--development-open-relay",
            ]));
        }
        assert!(!rejected(&scratch, values).await.is_empty());
    }
}
