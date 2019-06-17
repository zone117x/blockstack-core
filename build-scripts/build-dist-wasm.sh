#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

cd "$(dirname "$(dirname "$0")")"

apt-get update
apt-get install -y clang

git clone https://github.com/emscripten-core/emsdk.git
cd emsdk
./emsdk install latest
./emsdk activate latest
source ./emsdk_env.sh
cd ..

rustup target add wasm32-unknown-unknown

CC=emcc \
CXX=emcc \
CC_wasm32_unknown_unknown=emcc \
CARGO_TARGET_WASM32_UNKOWN_UNKOWN_LINKER=emcc \
cargo build --lib --target wasm32-unknown-unknown --release

