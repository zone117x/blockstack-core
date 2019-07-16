#!/bin/bash


### Working on MacOS with commands:
# cd $HOME
# git clone https://github.com/emscripten-core/emsdk
# cd emsdk
# ./emsdk install sdk-1.38.31-64bit
# ./emsdk activate sdk-1.38.31-64bit

### Environment
# EMSDK = $HOME/emsdk
# EM_CONFIG = $HOME/.emscripten
# LLVM_ROOT = $HOME/emsdk/clang/e1.38.31_64bit
# EMSCRIPTEN_NATIVE_OPTIMIZER = $HOME/emsdk/clang/e1.38.31_64bit/optimizer
# BINARYEN_ROOT = $HOME/emsdk/clang/e1.38.31_64bit/binaryen
# EMSDK_NODE = $HOME/emsdk/node/8.9.1_64bit/bin/node
# EMSCRIPTEN = $HOME/emsdk/emscripten/1.38.31
# emcc (Emscripten gcc/clang-like replacement + linker emulating GNU ld) 1.38.31
# clang version 6.0.1  (emscripten 1.38.31 : 1.38.31)
# Target: x86_64-apple-darwin18.6.0
# Thread model: posix
# InstalledDir: $HOME/emsdk/clang/e1.38.31_64bit

set -e

source "$HOME/emsdk/emsdk_env.sh"
emcc -v

cd "$(dirname "$(dirname "$0")")"

cargo clean
cargo web start --bin clarity-wasm --target wasm32-unknown-emscripten --release --host 127.0.0.1 --open