#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

cd "$(dirname "$(dirname "$0")")"

apt-get update
apt-get install -y nodejs cmake git clang

# rustup target add wasm32-unknown-emscripten

# cargo build --lib --target wasm32-unknown-emscripten --release

cargo install cargo-web
cargo web build --target wasm32-unknown-emscripten