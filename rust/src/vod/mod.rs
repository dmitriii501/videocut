//! VOD pipeline: download a recorded stream, pick highlights, transcribe,
//! describe and render vertical reels. The entry point is
//! [`pipeline::run_job`]; the rest of the stages are internal.

pub mod describe;
pub mod fetch;
pub mod highlights;
pub mod pipeline;
pub mod render;
pub mod webcam;
