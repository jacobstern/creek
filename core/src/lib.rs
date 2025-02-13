#![warn(rust_2018_idioms)]
#![warn(rust_2021_compatibility)]
// TODO: #![warn(clippy::missing_panics_doc)]
#![warn(clippy::clone_on_ref_ptr)]
#![deny(trivial_numeric_casts)]

use std::time;

pub mod read;
pub mod write;

pub use read::{DataBlock, Decoder, ReadDiskStream, ReadStreamOptions, SeekMode};
pub use write::{Encoder, WriteBlock, WriteDiskStream, WriteStatus, WriteStreamOptions};

const SERVER_WAIT_TIME: time::Duration = time::Duration::from_millis(1);

/// Info about the file/files.
#[derive(Clone)]
pub struct FileInfo<FileParams> {
    /// The total number of frames in the file/files.
    pub num_frames: usize,
    /// The number of channels in the file/files.
    pub num_channels: u16,
    /// The sample rate of the file/files (if it exists).
    pub sample_rate: Option<u32>,

    /// Additional info provided by the encoder/decoder.
    pub params: FileParams,
}
