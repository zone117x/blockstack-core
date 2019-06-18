#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

cd "$(dirname "$(dirname "$0")")"

apt-get update
apt-get install -y cmake git clang

install_deps () {
    apt-get update
    apt-get install -y --no-install-recommends \
        ca-certificates \
        g++ \
        make \
        gcc \
        libc6-dev \
        cmake \
        xz-utils
}
install_deps

install_nodejs () {
    curl -sL https://deb.nodesource.com/setup_12.x | bash -
    apt-get update
    apt-get install -y nodejs
}
install_nodejs

# install_emsdk_regular () {
#     git clone https://github.com/emscripten-core/emsdk.git
#     cd emsdk
#     git pull
#     ./emsdk install latest-upstream
#     ./emsdk activate latest-upstream
#     source ./emsdk_env.sh
#     cd ..
# }
# install_emsdk_regular

install_emsdk_portable_incoming() {
    curl -O https://s3.amazonaws.com/mozilla-games/emscripten/releases/emsdk-portable.tar.gz
    tar -xzf emsdk-portable.tar.gz
    source ./emsdk-portable/emsdk_env.sh
    emsdk update
    emsdk install sdk-incoming-64bit
    emsdk activate sdk-incoming-64bit
}
# install_emsdk_portable_incoming

install_emsdk_portable_ver() {
    ## copy setup from 
    ###  https://github.com/rust-lang/libc/blob/master/ci/emscripten.sh
    ###  https://github.com/rust-lang/libc/blob/master/ci/docker/wasm32-unknown-emscripten/Dockerfile
    curl --retry 5 -L https://s3.amazonaws.com/mozilla-games/emscripten/releases/emsdk-portable.tar.gz | \
    tar -xz
    source ./emsdk-portable/emsdk_env.sh
    emsdk update
    emsdk list
    emsdk install sdk-1.38.31-64bit
    emsdk activate sdk-1.38.31-64bit
}
install_emsdk_portable_ver
source ./emsdk-portable/emsdk_env.sh

# rustup target add wasm32-unknown-emscripten

export CC=emcc
export CXX=emcc
export CC_wasm32_unknown_emscripten=emcc
export CARGO_TARGET_WASM32_UNKOWN_EMSCRIPTEN_LINKER=emcc

cargo install cargo-web || true
cargo web build --bin clarity-cli --target wasm32-unknown-emscripten --use-system-emscripten
# --release