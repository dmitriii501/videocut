mod bot;
mod chat;
mod cli;
mod config;
mod describe;
mod fetch;
mod highlights;
mod http;
mod live;
mod live_pipeline;
mod media;
mod moment;
mod pipeline;
mod render;
mod review;
mod subtitles;
mod telegram;
mod translate;
mod utils;
mod webcam;
mod web;

fn main() {
    std::process::exit(cli::main());
}
