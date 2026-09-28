# divi-lock

Anchor program. Locks a Token-2022 mint in a PDA-owned vault until an unlock timestamp set at initialization; the beneficiary can only move the date later. Transfers pass a `max_fee` ceiling and forward transfer-hook accounts.

Program ID: `48s9SoST2ZA5AoW6ytBggQiExFFB16f1TJiWVauLUCbB`

## Build

```
cargo build-sbf --manifest-path programs/test-hook/Cargo.toml
cargo build-sbf --manifest-path programs/divi-lock/Cargo.toml
cargo test --manifest-path programs/divi-lock/Cargo.toml
```

Reproducible build:

```
solana-verify build --library-name divi_lock --base-image solanafoundation/solana-verifiable-build:4.2.2
```
