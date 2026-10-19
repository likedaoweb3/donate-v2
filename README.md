# Donate — deployed Solana program source

This repository contains the source reconstructed from the original modification records and independently rebuilt against the immutable mainnet Donate program.

- Program: `9NhbVKzzSKwkP7kEgNzSpacYA9UfCi2b58E1mrn2QsKT`
- ProgramData: `AGmC75QMtvKQLGa42pR8iShbu2d5JP3tRFG2SBjE4DRW`
- Effective executable size: **1,534,600 bytes**
- SHA-256: **`91fd6d202135deb006c0b48f9fbd0f1546901393a96d08ba647ad21c9331719d`**
- Deployment slot: `455540114`
- Upgrade authority: **None**

The reconstructed executable was compared byte for byte with finalized mainnet data on 2026-10-19, slots `458751352`–`458751365`. See [verification/mainnet-proof.json](verification/mainnet-proof.json). ProgramData additionally contains 35,520 zero padding bytes; these are excluded from the executable hash above.

The deployed source includes the LIKE tick routing repair and the daily treasury burn floor of 1,000,000 tokens. It excludes the unpublished SOL V2/compact instruction. The `dex-pool` crate is included because Donate depends on its CPI interface; this repository does not assert that this dependency is a verified build of the separate live DEX program.

## Build

Use Solana/Agave `2.1.0`, platform-tools `v1.43`, SBF Rust `1.79.0`, and the checked-in Cargo.lock. Anchor dependencies are `0.31.1`. Default features are used; do not enable `mainnet-test-10m`.

```sh
./scripts/build.sh
```

The exact build has been reproduced on **macOS arm64**, matching the original deployment environment. Use the official macOS aarch64 platform-tools v1.43 distribution. Standard-library file hashes are recorded in [verification/sbf-stdlib-sha256.json](verification/sbf-stdlib-sha256.json).

The standard Linux Docker image `solanafoundation/solana-verifiable-build:2.1.0` does **not** produce the deployed binary. Using the original macOS SBF target libraries inside Linux fixes the embedded source-path/length difference but still does not produce an identical executable. These unsuccessful Linux experiments are recorded in [verification/build-results.json](verification/build-results.json); no binary patching was used. Platform certification therefore remains pending manual review of the original build environment.

Build commands perform no deployment and require no wallet or private RPC credentials. The IDL is in [idl/donate.json](idl/donate.json).

## Verification status

Byte matching is evidence that the supplied source builds to the deployed program. It is not a security audit or a guarantee of uninterrupted business operation. Explorer/OtterSec certification is a separate process and is not claimed by this source publication.

Because the program is immutable, the ordinary upgrade-authority attestation cannot be created. The [official verification FAQ](https://solana.com/docs/programs/verified-builds#my-program-is-immutable-no-upgrade-authority-how-can-i-verify-it) describes the whitelisted verification process for this case.

## License

MIT. See [LICENSE](LICENSE).
