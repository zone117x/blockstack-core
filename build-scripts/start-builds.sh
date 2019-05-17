#!/bin/bash

### This is intended to run within the rust-stretch docker image (or a debian-like system with required dependencies).

script_path="$(dirname "$0")"
src_dir="$(dirname "$script_path")"
cd "$src_dir"

"$script_path/build-dist-x64-linux-gnu.sh"
"$script_path/build-dist-x64-linux-musl.sh"
"$script_path/build-dist-x64-windows-gnu.sh"
"$script_path/build-dist-x64-macos.sh"
