<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright (c) 2026 HaiyangLi -->

# khive-lion-core

This crate copies [`lion-core` 0.4.0](https://crates.io/crates/lion-core/0.4.0),
licensed under [Apache-2.0](LICENSE), into the khive workspace. The source is the
[published crates.io archive](https://static.crates.io/crates/lion-core/lion-core-0.4.0.crate),
whose SHA-256 is
`e22fa94b3007683a3e7c4772482e376c9af0949d74e8f7bc37fa2553da6dbe58`.
The upstream copyright notices, package version `0.4.0`, library name `lion_core`,
dependency requirements and features are retained. The package name is
`khive-lion-core` so khive can maintain this copy independently.

## Local addition

`Kernel::with_key(key_bytes: [u8; 32]) -> Result<Kernel, Error>` creates an empty
kernel using a caller-supplied process-local seal key. An all-zero key returns
`Error::Kernel(KernelError::InvalidCapability(...))` with the message
`seal key must not be all zero`. A successful construction starts at key epoch
zero, has no previous key, and preserves the empty state and deny-all policy.

The caller must obtain the key from a cryptographically suitable random source
and keep it private. The constructor performs no I/O or key generation. It checks
only for an all-zero value; accepting a key does not establish its entropy or
randomness. The local constructor is an addition to the published API and is not
claimed to have an upstream formal proof.

The rest of the origin API and implementation are unchanged, including
`Kernel::new()` and `Default`, which retain the upstream zero placeholder key.
Use `with_key` for new authority-bearing kernels. This crate does not provide
cross-process key provisioning or actor signatures.

The copied source provides delegated capability sealing and seal verification
through `Kernel::state().kernel().verify_cap_seal(...)`. The process-local
bootstrap root may use the existing raw-insertion API; a delegated grant uses
`Kernel::delegate_cap`. Raw insertion itself does not mint a seal.

## Dependency name

The Rust library name remains `lion_core`. A dependent khive crate can preserve
that import name with:

```toml
lion-core = { package = "khive-lion-core", version = "0.4.0", path = "../khive-lion-core" }
```
