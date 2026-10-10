set shell := ["bash", "-cu"]

default:
  @just --list

build:
  cargo build --release

check:
  cargo check
  cargo clippy

fmt:
  cargo fmt --all
  nixfmt **/*.nix

test:
  cargo test
