//! Live pipeline: watch a live stream, follow chat and audio for hype, and cut
//! reels as it happens. The entry point is [`live_pipeline::run_live`]; the
//! ring buffer, chat monitor and review log are internal.

pub mod chat;
pub mod live;
pub mod live_pipeline;
pub mod review;
