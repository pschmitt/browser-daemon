set shell := ["bash", "-cu"]

default:
  @just --list

build:
  cargo build --release

check:
  cargo check
  cargo clippy --all-targets -- -D warnings
  cargo fmt --all --check

fmt:
  cargo fmt --all
  nixfmt flake.nix nix/*.nix

test:
  cargo test
