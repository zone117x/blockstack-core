#!/bin/bash

script_path="$(dirname "$0")"
src_dir="$(dirname "$script_path")"
cd "$src_dir"
docker run --volume `pwd`:/build --workdir /build --tty rust:1.34-stretch bash "$script_path/start-builds.sh"
