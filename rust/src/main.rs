mod bot;
mod cli;
mod config;
mod http;
mod livestream;
mod media;
mod telegram;
mod text;
mod utils;
mod vod;
mod web;

fn main() {
    std::process::exit(cli::main());
}
