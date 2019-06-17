/*
 copyright: (c) 2013-2018 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

#![allow(unused_imports)]
#![allow(unused_assignments)]
#![allow(unused_variables)]
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

extern crate rand_os;
extern crate rand;
extern crate ini;
extern crate secp256k1;
extern crate serde;
extern crate serde_json;
extern crate rusqlite;
extern crate curve25519_dalek;
extern crate ed25519_dalek;
extern crate sha2;
extern crate sha3;
extern crate ripemd160;
extern crate dirs;
extern crate regex;
extern crate byteorder;

#[cfg(not(target_arch = "wasm32"))]
extern crate mio;

#[macro_use] extern crate serde_derive;

#[macro_use]
mod util;

#[macro_use]
mod chainstate;

mod address;
mod burnchains;
mod core;
mod deps;
mod net;
mod vm;

mod clarity;

use std::fs;
use std::env;
use std::process;

use util::log;

fn main() {
    log::set_loglevel(log::LOG_DEBUG).unwrap();
    let argv : Vec<String> = env::args().collect();

    clarity::invoke_command(&argv[0], &argv[1..]);
}
