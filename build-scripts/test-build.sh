#!/bin/bash


## Working on Ubuntu 18.04 with ./emsdk activate sdk-1.38.31-64bit -- 

# Setting environment variables:
# EMSDK = /home/matt/emsdk
# EM_CONFIG = /home/matt/.emscripten
# LLVM_ROOT = /home/matt/emsdk/clang/e1.38.31_64bit
# EMSCRIPTEN_NATIVE_OPTIMIZER = /home/matt/emsdk/clang/e1.38.31_64bit/optimizer
# BINARYEN_ROOT = /home/matt/emsdk/clang/e1.38.31_64bit/binaryen
# EMSDK_NODE = /home/matt/emsdk/node/8.9.1_64bit/bin/node
# EMSCRIPTEN = /home/matt/emsdk/emscripten/1.38.31

# emcc (Emscripten gcc/clang-like replacement + linker emulating GNU ld) 1.38.31
# clang version 6.0.1  (emscripten 1.38.31 : 1.38.31)
# Target: x86_64-unknown-linux-gnu
# Thread model: posix
# InstalledDir: /home/matt/emsdk/clang/e1.38.31_64bit
# Found candidate GCC installation: /usr/lib/gcc/x86_64-linux-gnu/7
# Found candidate GCC installation: /usr/lib/gcc/x86_64-linux-gnu/7.4.0
# Found candidate GCC installation: /usr/lib/gcc/x86_64-linux-gnu/8
# Selected GCC installation: /usr/lib/gcc/x86_64-linux-gnu/7.4.0
# Candidate multilib: .;@m64
# Selected multilib: .;@m64


set -e

source "$HOME/emsdk/emsdk_env.sh"
emcc -v


cd "$(dirname "$(dirname "$0")")"
export CC=emcc
export CXX=emcc
export CC_wasm32_unknown_emscripten=emcc
export CARGO_TARGET_WASM32_UNKOWN_EMSCRIPTEN_LINKER=emcc
export CARGO_TARGET_WASM32_UNKOWN_EMSCRIPTEN_AR=emcc

export RUSTFLAGS="-C link-arg=-O0 -C link-arg=-oclarity.html"
export EMMAKEN_CFLAGS="-s ERROR_ON_UNDEFINED_SYMBOLS=0"

cargo build --bin clarity-cli --target wasm32-unknown-emscripten --release