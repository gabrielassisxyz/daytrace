mod activity;
mod cli;
mod config;
mod desktop;
mod export;
mod input;
mod lock;
mod media;
mod service;
mod session;
mod storage;
mod timeline;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main_exit()
}
