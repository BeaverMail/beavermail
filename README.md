# BeaverMail client

Desktop mail client written in Rust with [egui](https://github.com/emilk/egui) (eframe).
Licensed under AGPL-3.0-only (see [LICENSE](LICENSE)). Product requirements are in
[docs/prd.md](docs/prd.md).

## Layout

- `crates/core`: domain model and the `MailSource` trait. No GUI or network dependencies.
- `crates/app`: the egui desktop shell (binary `beavermail`).

## Status

Milestone 0: a three-pane window (folders, message list, reader) over generated sample
data. No protocols, storage or encryption yet.

## Running

```sh
cargo run -p beavermail
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
