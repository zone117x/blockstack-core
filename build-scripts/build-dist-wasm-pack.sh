#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

cd "$(dirname "$(dirname "$0")")"

# wget -O - https://apt.llvm.org/llvm-snapshot.gpg.key | apt-key add - 
# echo "deb http://apt.llvm.org/stretch/ llvm-toolchain-stretch-8 main" | tee -a /etc/apt/sources.list
# echo "deb-src http://apt.llvm.org/stretch/ llvm-toolchain-stretch-8 main" | tee -a /etc/apt/sources.list

# apt-get update
# apt-get install -y --no-install-recommends ca-certificates clang-8 lldb-8 lld-8
# apt-get install -y ca-certificates libssl-dev clang-8 lldb-8 lld-8

rustup target add wasm32-unknown-unknown

# cargo build --lib --target wasm32-unknown-unknown --release

# curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
# cargo install wasm-pack --force
cargo install wasm-pack

CC=emcc wasm-pack build --dev -- -v
