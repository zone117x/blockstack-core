#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

set -e

cd "$(dirname "$(dirname "$0")")"

apt-get update
apt-get install -y cmake git clang

install_nodejs () {
    curl -sL https://deb.nodesource.com/setup_12.x | bash -
    apt-get update
    apt-get install -y nodejs
}

install_deps () {
    apt-get update
    apt-get install -y --no-install-recommends \
        ca-certificates \
        g++ \
        make \
        file \
        curl \
        gcc \
        git \
        libc6-dev \
        python \
        cmake \
        gdb \
        xz-utils
}

install_emsdk_regular () {
    # git clone https://github.com/emscripten-core/emsdk.git
    cd emsdk
    git pull

    # ./emsdk install sdk-incoming-64bit
    # ./emsdk activate sdk-incoming-64bit

    # ./emsdk install latest
    # ./emsdk activate latest

    ./emsdk install latest-upstream
    ./emsdk activate latest-upstream

    source ./emsdk_env.sh
    cd ..
}

install_emsdk_portable () {

    install_deps

    ## copy setup from 
    ###  https://github.com/rust-lang/libc/blob/master/ci/emscripten.sh
    ###  https://github.com/rust-lang/libc/blob/master/ci/docker/wasm32-unknown-emscripten/Dockerfile
    curl --retry 5 -L https://s3.amazonaws.com/mozilla-games/emscripten/releases/emsdk-portable.tar.gz | \
    tar -xz
    cd emsdk-portable
    ./emsdk update
    ./emsdk install sdk-1.38.31-64bit
    ./emsdk activate sdk-1.38.31-64bit
    source ./emsdk_env.sh
    cd ..
}

# install_emsdk_portable

install_nodejs
install_emsdk_regular

export CC=emcc
export CXX=emcc
export CC_wasm32_unknown_unknown=emcc
export CARGO_TARGET_WASM32_UNKOWN_UNKOWN_LINKER=emcc

rustup target add wasm32-unknown-emscripten

cargo build --lib --target wasm32-unknown-emscripten --release

