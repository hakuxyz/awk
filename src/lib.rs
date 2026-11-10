//! POSIX awk in Rust: lexer, parser, interpreter.
//! Drop-in CLI lives in main.rs; this crate exposes the engine.

pub mod ast;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod value;

pub use interp::Interp;
pub use value::Value;
