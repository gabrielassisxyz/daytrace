mod activity;
mod cli;
mod config;
mod desktop;
mod export;
mod input;
mod service;
mod storage;
mod timeline;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main_exit()
}
