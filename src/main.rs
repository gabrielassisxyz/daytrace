mod activity;
mod cli;
mod config;
mod desktop;
mod input;
mod storage;
mod timeline;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main_exit()
}
