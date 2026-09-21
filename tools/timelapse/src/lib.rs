//! Mac-side timelapse builder for Project Optic captures
//! (`docs/timelapse-builder.md`). Everything here is pure and unit-tested;
//! filesystem and ffmpeg access live in `main.rs`.

pub mod cli;
pub mod encode;
pub mod names;
pub mod plan;
