//! GoldSrc wire primitives, reimplemented in Rust.
//!
//! This is a clean reimplementation of the `internal/proto` package from
//! `aiplayers-gui.exe` (Go 1.26.3, stripped). Every constant and every
//! algorithm here was recovered from that binary's disassembly and is cited in
//! the module it lives in, so behaviour can be re-derived rather than trusted.
//!
//! Layer status:
//!
//! | module   | origin                      | state |
//! |----------|-----------------------------|-------|
//! | `bitbuf`         | `internal/proto/bitbuf.go`    | done, except `read_bit_vec3_coord` (see note) |
//! | `munge`          | `internal/proto/munge.go`     | done, tables + key formula extracted |
//! | `crc`            | `internal/proto/crc.go`       | done |
//! | `delta`          | `internal/proto/delta.go`     | framing done; numeric conversions pending live validation |
//! | `usercmd`        | `internal/proto/usercmd.go`   | done (outer `clc_move` framing belongs to the client layer) |
//! | `resources`      | `internal/proto/resources.go` | MD5 + types done; wire layout pending a live capture |
//! | `connectionless` | `internal/client/client.go`   | done, validated against a live server |
//!
//! Still to port: `client`, `nav`, `bot`, `proxy`, `runner` and the GUI.
//! The netchannel lives in the sibling `netchan` crate.

pub mod bitbuf;
pub mod connectionless;
pub mod consistency;
pub mod crc;
pub mod delta;
pub mod munge;
pub mod resources;
pub mod usercmd;

pub use bitbuf::{BitReader, BitWriter};
pub use delta::{DeltaRegistry, FieldDesc, FieldMask, Value};
