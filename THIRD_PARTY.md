# Third-party software

Leviathan links the crates below into its binary (`cargo tree -p leviathan-index -e normal --target all`, default features). SQLite itself is public domain and is compiled in by `libsqlite3-sys`.
The `remote` feature (default) adds the HTTP and TLS crates (`tiny_http`, `ureq`, `rustls`, `ring`, `webpki-roots` with Mozilla's CA list under CDLA-Permissive-2.0, `sha2`, `base64`); `--no-default-features` leaves them out.

The benchmark charts and banner use [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) 2.304,
bundled in `bench/fonts/` under the SIL Open Font License 1.1 (`bench/fonts/OFL.txt`). It is not part of the binary.

| Crate | Version | License |
|---|---|---|
| [adler2](https://crates.io/crates/adler2) | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| [aho-corasick](https://crates.io/crates/aho-corasick) | 1.1.5 | Unlicense OR MIT |
| [anstream](https://crates.io/crates/anstream) | 1.0.0 | MIT OR Apache-2.0 |
| [anstyle-parse](https://crates.io/crates/anstyle-parse) | 1.0.0 | MIT OR Apache-2.0 |
| [anstyle-query](https://crates.io/crates/anstyle-query) | 1.1.5 | MIT OR Apache-2.0 |
| [anstyle](https://crates.io/crates/anstyle) | 1.0.14 | MIT OR Apache-2.0 |
| [anstyle-wincon](https://crates.io/crates/anstyle-wincon) | 3.0.11 | MIT OR Apache-2.0 |
| [anyhow](https://crates.io/crates/anyhow) | 1.0.104 | MIT OR Apache-2.0 |
| [ascii](https://crates.io/crates/ascii) | 1.1.0 | Apache-2.0 OR MIT |
| [base64](https://crates.io/crates/base64) | 0.23.1 | MIT OR Apache-2.0 |
| [bitflags](https://crates.io/crates/bitflags) | 2.13.2 | MIT OR Apache-2.0 |
| [block-buffer](https://crates.io/crates/block-buffer) | 0.12.1 | MIT OR Apache-2.0 |
| [bumpalo](https://crates.io/crates/bumpalo) | 3.20.3 | MIT OR Apache-2.0 |
| [bytes](https://crates.io/crates/bytes) | 1.12.1 | MIT |
| [cfg-if](https://crates.io/crates/cfg-if) | 1.0.5 | MIT OR Apache-2.0 |
| [chunked_transfer](https://crates.io/crates/chunked_transfer) | 1.5.0 | MIT OR Apache-2.0 |
| [clap_builder](https://crates.io/crates/clap_builder) | 4.6.7 | MIT OR Apache-2.0 |
| [clap_derive](https://crates.io/crates/clap_derive) | 4.6.7 | MIT OR Apache-2.0 |
| [clap_lex](https://crates.io/crates/clap_lex) | 1.1.1 | MIT OR Apache-2.0 |
| [clap](https://crates.io/crates/clap) | 4.6.7 | MIT OR Apache-2.0 |
| [colorchoice](https://crates.io/crates/colorchoice) | 1.0.5 | MIT OR Apache-2.0 |
| [const-oid](https://crates.io/crates/const-oid) | 0.10.2 | Apache-2.0 OR MIT |
| [cpufeatures](https://crates.io/crates/cpufeatures) | 0.3.1 | MIT OR Apache-2.0 |
| [crc32fast](https://crates.io/crates/crc32fast) | 1.5.2 | MIT OR Apache-2.0 |
| [crypto-common](https://crates.io/crates/crypto-common) | 0.2.2 | MIT OR Apache-2.0 |
| [csv-core](https://crates.io/crates/csv-core) | 0.1.13 | Unlicense/MIT |
| [csv](https://crates.io/crates/csv) | 1.4.0 | Unlicense/MIT |
| [digest](https://crates.io/crates/digest) | 0.11.3 | MIT OR Apache-2.0 |
| [equivalent](https://crates.io/crates/equivalent) | 1.0.2 | Apache-2.0 OR MIT |
| [fallible-iterator](https://crates.io/crates/fallible-iterator) | 0.3.0 | MIT/Apache-2.0 |
| [fallible-streaming-iterator](https://crates.io/crates/fallible-streaming-iterator) | 0.1.9 | MIT/Apache-2.0 |
| [flate2](https://crates.io/crates/flate2) | 1.1.10 | MIT OR Apache-2.0 |
| [foldhash](https://crates.io/crates/foldhash) | 0.2.0 | Zlib |
| [getrandom](https://crates.io/crates/getrandom) | 0.2.17 | MIT OR Apache-2.0 |
| [getrandom](https://crates.io/crates/getrandom) | 0.3.4 | MIT OR Apache-2.0 |
| [hashbrown](https://crates.io/crates/hashbrown) | 0.16.1 | MIT OR Apache-2.0 |
| [hashbrown](https://crates.io/crates/hashbrown) | 0.17.1 | MIT OR Apache-2.0 |
| [hashlink](https://crates.io/crates/hashlink) | 0.12.2 | MIT OR Apache-2.0 |
| [heck](https://crates.io/crates/heck) | 0.5.0 | MIT OR Apache-2.0 |
| [httparse](https://crates.io/crates/httparse) | 1.10.1 | MIT OR Apache-2.0 |
| [httpdate](https://crates.io/crates/httpdate) | 1.0.3 | MIT OR Apache-2.0 |
| [http](https://crates.io/crates/http) | 1.5.0 | MIT OR Apache-2.0 |
| [hybrid-array](https://crates.io/crates/hybrid-array) | 0.4.15 | MIT OR Apache-2.0 |
| [indexmap](https://crates.io/crates/indexmap) | 2.14.2 | Apache-2.0 OR MIT |
| [is_terminal_polyfill](https://crates.io/crates/is_terminal_polyfill) | 1.70.2 | MIT OR Apache-2.0 |
| [itoa](https://crates.io/crates/itoa) | 1.0.18 | MIT OR Apache-2.0 |
| [js-sys](https://crates.io/crates/js-sys) | 0.3.106 | MIT OR Apache-2.0 |
| [libc](https://crates.io/crates/libc) | 0.2.190 | MIT OR Apache-2.0 |
| [libsqlite3-sys](https://crates.io/crates/libsqlite3-sys) | 0.38.2 | MIT |
| [log](https://crates.io/crates/log) | 0.4.34 | MIT OR Apache-2.0 |
| [memchr](https://crates.io/crates/memchr) | 2.8.3 | Unlicense OR MIT |
| [miniz_oxide](https://crates.io/crates/miniz_oxide) | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| [once_cell_polyfill](https://crates.io/crates/once_cell_polyfill) | 1.70.2 | MIT OR Apache-2.0 |
| [once_cell](https://crates.io/crates/once_cell) | 1.21.4 | MIT OR Apache-2.0 |
| [percent-encoding](https://crates.io/crates/percent-encoding) | 2.3.2 | MIT OR Apache-2.0 |
| [proc-macro2](https://crates.io/crates/proc-macro2) | 1.0.107 | MIT OR Apache-2.0 |
| [quote](https://crates.io/crates/quote) | 1.0.47 | MIT OR Apache-2.0 |
| [r-efi](https://crates.io/crates/r-efi) | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| [regex-automata](https://crates.io/crates/regex-automata) | 0.4.18 | MIT OR Apache-2.0 |
| [regex-syntax](https://crates.io/crates/regex-syntax) | 0.8.11 | MIT OR Apache-2.0 |
| [regex](https://crates.io/crates/regex) | 1.13.1 | MIT OR Apache-2.0 |
| [ring](https://crates.io/crates/ring) | 0.17.14 | Apache-2.0 AND ISC |
| [rsqlite-vfs](https://crates.io/crates/rsqlite-vfs) | 0.1.1 | MIT |
| [rusqlite](https://crates.io/crates/rusqlite) | 0.40.2 | MIT |
| [rustls-pki-types](https://crates.io/crates/rustls-pki-types) | 1.15.1 | MIT OR Apache-2.0 |
| [rustls](https://crates.io/crates/rustls) | 0.23.45 | Apache-2.0 OR ISC OR MIT |
| [rustls-webpki](https://crates.io/crates/rustls-webpki) | 0.103.15 | ISC |
| [ryu](https://crates.io/crates/ryu) | 1.0.23 | Apache-2.0 OR BSL-1.0 |
| [serde_core](https://crates.io/crates/serde_core) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_derive](https://crates.io/crates/serde_derive) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_json](https://crates.io/crates/serde_json) | 1.0.151 | MIT OR Apache-2.0 |
| [serde_spanned](https://crates.io/crates/serde_spanned) | 1.1.1 | MIT OR Apache-2.0 |
| [serde](https://crates.io/crates/serde) | 1.0.229 | MIT OR Apache-2.0 |
| [sha2](https://crates.io/crates/sha2) | 0.11.0 | MIT OR Apache-2.0 |
| [simd-adler32](https://crates.io/crates/simd-adler32) | 0.3.10 | MIT |
| [smallvec](https://crates.io/crates/smallvec) | 1.16.2 | MIT OR Apache-2.0 |
| [sqlite-wasm-rs](https://crates.io/crates/sqlite-wasm-rs) | 0.5.5 | MIT |
| [strsim](https://crates.io/crates/strsim) | 0.11.1 | MIT |
| [subtle](https://crates.io/crates/subtle) | 2.6.1 | BSD-3-Clause |
| [syn](https://crates.io/crates/syn) | 3.0.6 | MIT OR Apache-2.0 |
| [thiserror-impl](https://crates.io/crates/thiserror-impl) | 2.0.21 | MIT OR Apache-2.0 |
| [thiserror](https://crates.io/crates/thiserror) | 2.0.21 | MIT OR Apache-2.0 |
| [tiny_http](https://crates.io/crates/tiny_http) | 0.12.0 | MIT OR Apache-2.0 |
| [toml_datetime](https://crates.io/crates/toml_datetime) | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml_parser](https://crates.io/crates/toml_parser) | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml](https://crates.io/crates/toml) | 1.1.6+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml_writer](https://crates.io/crates/toml_writer) | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 |
| [typenum](https://crates.io/crates/typenum) | 1.20.1 | MIT OR Apache-2.0 |
| [unicode-ident](https://crates.io/crates/unicode-ident) | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [untrusted](https://crates.io/crates/untrusted) | 0.9.0 | ISC |
| [ureq-proto](https://crates.io/crates/ureq-proto) | 0.6.4 | MIT OR Apache-2.0 |
| [ureq](https://crates.io/crates/ureq) | 3.4.2 | MIT OR Apache-2.0 |
| [utf8parse](https://crates.io/crates/utf8parse) | 0.2.2 | Apache-2.0 OR MIT |
| [utf8-zero](https://crates.io/crates/utf8-zero) | 0.8.1 | MIT OR Apache-2.0 |
| [wasip2](https://crates.io/crates/wasip2) | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [wasi](https://crates.io/crates/wasi) | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [wasm-bindgen-macro-support](https://crates.io/crates/wasm-bindgen-macro-support) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen-macro](https://crates.io/crates/wasm-bindgen-macro) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen-shared](https://crates.io/crates/wasm-bindgen-shared) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen](https://crates.io/crates/wasm-bindgen) | 0.2.129 | MIT OR Apache-2.0 |
| [webpki-roots](https://crates.io/crates/webpki-roots) | 1.0.9 | CDLA-Permissive-2.0 |
| [windows_aarch64_gnullvm](https://crates.io/crates/windows_aarch64_gnullvm) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_aarch64_msvc](https://crates.io/crates/windows_aarch64_msvc) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_gnullvm](https://crates.io/crates/windows_i686_gnullvm) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_gnu](https://crates.io/crates/windows_i686_gnu) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_msvc](https://crates.io/crates/windows_i686_msvc) | 0.52.6 | MIT OR Apache-2.0 |
| [windows-link](https://crates.io/crates/windows-link) | 0.2.1 | MIT OR Apache-2.0 |
| [windows-sys](https://crates.io/crates/windows-sys) | 0.52.0 | MIT OR Apache-2.0 |
| [windows-sys](https://crates.io/crates/windows-sys) | 0.61.2 | MIT OR Apache-2.0 |
| [windows-targets](https://crates.io/crates/windows-targets) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_gnullvm](https://crates.io/crates/windows_x86_64_gnullvm) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_gnu](https://crates.io/crates/windows_x86_64_gnu) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_msvc](https://crates.io/crates/windows_x86_64_msvc) | 0.52.6 | MIT OR Apache-2.0 |
| [winnow](https://crates.io/crates/winnow) | 1.0.4 | MIT |
| [wit-bindgen](https://crates.io/crates/wit-bindgen) | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [zeroize](https://crates.io/crates/zeroize) | 1.9.1 | Apache-2.0 OR MIT |
| [zmij](https://crates.io/crates/zmij) | 1.0.23 | MIT |
