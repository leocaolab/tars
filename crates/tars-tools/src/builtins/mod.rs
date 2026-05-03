//! Built-in tools shipped with `tars-tools`. Each is its own module
//! so a downstream consumer (or future `--enable-tools` flag) can opt
//! in selectively.

mod read_file;

pub use read_file::ReadFileTool;
