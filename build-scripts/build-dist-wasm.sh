#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

cd "$(dirname "$(dirname "$0")")"

apt-get update

rustup target add wasm32-unknown-unknown

cargo build --target wasm32-unknown-unknown --release

