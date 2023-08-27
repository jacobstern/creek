#![warn(rust_2018_idioms)]
#![warn(rust_2021_compatibility)]
#![warn(clippy::missing_panics_doc)]
#![warn(clippy::clone_on_ref_ptr)]
#![deny(trivial_numeric_casts)]

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use log::debug;
use symphonia::core::audio::AudioBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder as SymphDecoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{Metadata, MetadataOptions, MetadataRevision};
use symphonia::core::probe::Hint;

use creek_core::read::Decoder;
use creek_core::{AudioBlock, FileInfo};

mod error;
pub use error::OpenError;

/// A decoder for creek that reads from an audio file.
pub struct SymphoniaDecoder {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn SymphDecoder>,

    decode_buffer: AudioBuffer<f32>,
    decode_buffer_len: usize,
    curr_decode_buffer_frame: usize,

    num_frames: usize,
    block_frames: usize,

    playhead_frame: usize,
    reset_decode_buffer: bool,

    seek_delta: usize,
    default_track_id: u32,
}

impl Decoder for SymphoniaDecoder {
    type T = f32;
    type FileParams = SymphoniaDecoderInfo;
    type OpenError = OpenError;
    type FatalError = Error;
    type AdditionalOpts = ();

    const DEFAULT_BLOCK_FRAMES: usize = 16384;
    const DEFAULT_NUM_CACHE_BLOCKS: usize = 0;
    const DEFAULT_NUM_LOOK_AHEAD_BLOCKS: usize = 8;
    const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1);

    fn new(
        file: PathBuf,
        start_frame: usize,
        block_frames: usize,
        _poll_interval: Duration,
        _additional_opts: Self::AdditionalOpts,
    ) -> Result<(Self, FileInfo<Self::FileParams>), Self::OpenError> {
        // Create a hint to help the format registry guess what format reader is appropriate.
        let mut hint = Hint::new();

        // Provide the file extension as a hint.
        if let Some(extension) = file.extension() {
            if let Some(extension_str) = extension.to_str() {
                hint.with_extension(extension_str);
            }
        }

        let source = Box::new(File::open(file)?);

        // Create the media source stream using the boxed media source from above.
        let mss = MediaSourceStream::new(source, Default::default());

        // Use the default options for metadata and format readers.
        let format_opts: FormatOptions = Default::default();
        let metadata_opts: MetadataOptions = Default::default();

        let probed =
            symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts)?;

        let mut reader = probed.format;

        let decoder_opts = DecoderOptions {
            ..Default::default()
        };

        let default_track = reader
            .default_track()
            .ok_or_else(|| OpenError::NoDefaultTrack)?;
        let track_id = default_track.id;
        let params = default_track.codec_params.clone();

        let num_frames = params.n_frames.ok_or_else(|| OpenError::NoNumFrames)? as usize;
        let sample_rate = params.sample_rate;
        let mut seek_delta = 0_usize;

        // Seek the reader to the requested position.
        if start_frame != 0 {
            let res = reader.seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: start_frame as u64,
                    track_id,
                },
            )?;
            seek_delta = start_frame - res.actual_ts as usize;
            debug!("Found seek delta of {} for initial seek", seek_delta);
        }

        // Create a decoder for the stream.
        let mut decoder = symphonia::default::get_codecs().make(&params, &decoder_opts)?;
        assert_eq!(params.n_frames, decoder.codec_params().n_frames);
        assert_eq!(params.sample_rate, decoder.codec_params().sample_rate);
        assert_eq!(params.channels, decoder.codec_params().channels);

        // The stream/decoder might not always provide the actual numbers
        // of channels (MP4/AAC/ALAC). In this case the number of channels
        // will be obtained from the signal spec of the first decoded packet.
        let mut channels = params.channels;

        // Decode the first packet to get the signal specification.
        let (decode_buffer, decode_buffer_len) = loop {
            match decoder.decode(&reader.next_packet()?) {
                Ok(decoded) => {
                    // Get the buffer spec.
                    let spec = *decoded.spec();
                    if let Some(channels) = channels {
                        assert_eq!(channels, spec.channels);
                    } else {
                        log::debug!(
                            "Assuming {num_channels} channel(s) according to the first decoded packet",
                            num_channels = spec.channels.count()
                        );
                        channels = Some(spec.channels);
                    }

                    let len = decoded.frames();
                    if seek_delta < len {
                        let decode_buffer: AudioBuffer<f32> = decoded.make_equivalent();

                        break (decode_buffer, len);
                    } else {
                        // Continue decoding to seek point
                        seek_delta -= len;
                    }
                }
                Err(Error::DecodeError(err)) => {
                    // Decode errors are not fatal.
                    log::warn!("{err}");
                    // If we skipped a packet, the seek delta is probably no longer accurate
                    seek_delta = 0;
                    // Continue by decoding the next packet.
                    continue;
                }
                Err(e) => {
                    // Errors other than decode errors are fatal.
                    return Err(e.into());
                }
            }
        };

        let metadata = reader.metadata().skip_to_latest().cloned();
        let info = SymphoniaDecoderInfo {
            codec_params: params,
            metadata,
        };
        let num_channels = (channels.ok_or_else(|| OpenError::NoNumChannels)?).count();

        let file_info = FileInfo {
            params: info,
            num_frames,
            num_channels: num_channels as u16,
            sample_rate,
        };
        Ok((
            Self {
                reader,
                decoder,

                decode_buffer,
                decode_buffer_len,
                curr_decode_buffer_frame: seek_delta,

                num_frames,
                block_frames,

                playhead_frame: start_frame,
                reset_decode_buffer: false,

                seek_delta: 0,
                default_track_id: track_id,
            },
            file_info,
        ))
    }

    fn seek(&mut self, frame: usize) -> Result<(), Self::FatalError> {
        if frame >= self.num_frames {
            // Do nothing if out of range.
            self.playhead_frame = self.num_frames;

            return Ok(());
        }

        self.playhead_frame = frame;

        match self.reader.seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: frame as u64,
                track_id: self.default_track_id,
            },
        ) {
            Ok(res) => {
                self.seek_delta = frame - res.actual_ts as usize;
                if self.seek_delta > 0 {
                    debug!("Found seek delta of {}", frame - res.actual_ts as usize);
                }
            }
            Err(e) => {
                return Err(e);
            }
        }

        self.reset_decode_buffer = true;
        self.curr_decode_buffer_frame = 0;

        /*
        let decoder_opts = DecoderOptions {
            verify: false,
            ..Default::default()
        };

        self.decoder.close();
        self.decoder = symphonia::default::get_codecs()
            .make(self.decoder.codec_params(), &decoder_opts)?;
            */

        Ok(())
    }

    fn decode(&mut self, block: &mut AudioBlock<Self::T>) -> Result<(), Self::FatalError> {
        if self.playhead_frame >= self.num_frames {
            // Fill with zeros if reached the end of the file.
            for ch in block.channels.iter_mut() {
                ch.fill(Default::default());
            }

            return Ok(());
        }

        let mut reached_end_of_file = false;

        let mut block_start_frame = 0;
        while block_start_frame < self.block_frames {
            let num_frames_to_cpy = if self.reset_decode_buffer {
                // Get new data first.
                self.reset_decode_buffer = false;
                0
            } else {
                // Find the maximum amount of frames that can be copied.
                (self.block_frames - block_start_frame)
                    .min(self.decode_buffer_len - self.curr_decode_buffer_frame)
            };

            if num_frames_to_cpy != 0 {
                let src_planes = self.decode_buffer.planes();
                let src_channels = src_planes.planes();

                for (dst_ch, src_ch) in block.channels.iter_mut().zip(src_channels) {
                    let src_ch_part = &src_ch[self.curr_decode_buffer_frame
                        ..self.curr_decode_buffer_frame + num_frames_to_cpy];
                    dst_ch[block_start_frame..block_start_frame + num_frames_to_cpy]
                        .copy_from_slice(src_ch_part);
                }

                block_start_frame += num_frames_to_cpy;

                self.curr_decode_buffer_frame += num_frames_to_cpy;
                if self.curr_decode_buffer_frame >= self.decode_buffer_len {
                    self.reset_decode_buffer = true;
                }
            } else {
                // Decode the next packet.

                loop {
                    match self.reader.next_packet() {
                        Ok(packet) => {
                            match self.decoder.decode(&packet) {
                                Ok(decoded) => {
                                    let seek_delta = self.seek_delta;
                                    let decoded_frames = decoded.frames();
                                    if seek_delta < decoded_frames {
                                        self.seek_delta = 0;
                                        self.decode_buffer_len = decoded_frames;
                                        decoded.convert(&mut self.decode_buffer);

                                        self.curr_decode_buffer_frame = seek_delta;
                                        if seek_delta > 0 {
                                            debug!("Recovered seek delta of {seek_delta}");
                                        }
                                    } else {
                                        // Continue until we decode back to the desired seek point
                                        self.seek_delta -= decoded_frames;
                                        debug!(
                                            "Skipped {} decoded frames, seek delta is now {}",
                                            decoded_frames, self.seek_delta
                                        );
                                    }
                                    break;
                                }
                                Err(Error::DecodeError(err)) => {
                                    // Decode errors are not fatal.
                                    log::warn!("{err}");
                                    // Continue by decoding the next packet.
                                    continue;
                                }
                                Err(e) => {
                                    // Errors other than decode errors are fatal.
                                    return Err(e);
                                }
                            }
                        }
                        Err(e) => {
                            if let Error::IoError(io_error) = &e {
                                if io_error.kind() == std::io::ErrorKind::UnexpectedEof {
                                    // End of file, stop decoding.
                                    reached_end_of_file = true;
                                    block_start_frame = self.block_frames;
                                    break;
                                } else {
                                    return Err(e);
                                }
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
            }
        }

        if reached_end_of_file {
            self.playhead_frame = self.num_frames;
        } else {
            self.playhead_frame += self.block_frames;
        }

        Ok(())
    }

    fn playhead_frame(&self) -> usize {
        self.playhead_frame
    }
}

impl Drop for SymphoniaDecoder {
    fn drop(&mut self) {
        let _ = self.decoder.finalize();
    }
}

impl SymphoniaDecoder {
    /// Symphonia does metadata oddly. This is more for raw access.
    ///
    /// See [`Metadata`](https://docs.rs/symphonia-core/0.5.2/symphonia_core/meta/struct.Metadata.html).
    pub fn get_metadata_raw(&mut self) -> Metadata<'_> {
        self.reader.metadata()
    }

    /// Get the latest entry in the metadata.
    pub fn get_metadata(&mut self) -> Option<MetadataRevision> {
        let mut md = self.reader.metadata();
        md.skip_to_latest().cloned()
    }
}

/// Information about the Symphonia decoder.
#[derive(Debug, Clone)]
pub struct SymphoniaDecoderInfo {
    /// Information about the audio codec.
    pub codec_params: CodecParameters,
    /// Metadata information in the file.
    pub metadata: Option<MetadataRevision>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use float_cmp::*;

    #[test]
    fn decoder_new() {
        let files = vec![
            //  file | num_channels | num_frames | sample_rate
            ("../test_files/wav_u8_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i16_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i24_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i32_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_f32_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i24_stereo.wav", 2, 1323000, Some(44100)),
            //"../test_files/ogg_mono.ogg",
            //"../test_files/ogg_stereo.ogg",
            //"../test_files/mp3_constant_mono.mp3",
            //"../test_files/mp3_constant_stereo.mp3",
            //"../test_files/mp3_variable_mono.mp3",
            //"../test_files/mp3_variable_stereo.mp3",
        ];

        for file in files {
            dbg!(file.0);
            let decoder = SymphoniaDecoder::new(
                file.0.into(),
                0,
                SymphoniaDecoder::DEFAULT_BLOCK_FRAMES,
                SymphoniaDecoder::DEFAULT_POLL_INTERVAL,
                (),
            );
            match decoder {
                Ok((_, file_info)) => {
                    assert_eq!(file_info.num_channels, file.1);
                    assert_eq!(file_info.num_frames, file.2);
                    //assert_eq!(file_info.sample_rate, file.3);
                }
                Err(e) => {
                    panic!("{}", e);
                }
            }
        }
    }

    #[test]
    fn decode_first_frame() {
        let block_frames = 10;

        let decoder = SymphoniaDecoder::new(
            "../test_files/wav_u8_mono.wav".into(),
            0,
            block_frames,
            SymphoniaDecoder::DEFAULT_POLL_INTERVAL,
            (),
        );

        let (mut decoder, file_info) = decoder.unwrap();

        let mut block = AudioBlock::new(1, block_frames);
        decoder.decode(&mut block).unwrap();

        let samples = &mut block.channels[0];
        assert_eq!(samples.len(), block_frames);

        let first_frame = [
            0.0, 0.046875, 0.09375, 0.1484375, 0.1953125, 0.2421875, 0.2890625, 0.3359375,
            0.3828125, 0.421875,
        ];

        for i in 0..samples.len() {
            assert!(approx_eq!(f32, first_frame[i], samples[i], ulps = 2));
        }

        let second_frame = [
            0.46875, 0.5078125, 0.5390625, 0.578125, 0.609375, 0.640625, 0.671875, 0.6953125,
            0.71875, 0.7421875,
        ];

        decoder.decode(&mut block).unwrap();

        let samples = &mut block.channels[0];
        for i in 0..samples.len() {
            assert_approx_eq!(f32, second_frame[i], samples[i], ulps = 2);
        }

        let last_frame = [
            -0.0625, -0.046875, -0.0234375, -0.0078125, 0.015625, 0.03125, 0.046875, 0.0625,
            0.078125, 0.0859375,
        ];

        // Seek to last frame
        decoder
            .seek(file_info.num_frames - 1 - block_frames)
            .unwrap();

        decoder.decode(&mut block).unwrap();
        let samples = &mut block.channels[0];
        for i in 0..samples.len() {
            assert_approx_eq!(f32, last_frame[i], samples[i], ulps = 2);
        }

        assert_eq!(decoder.playhead_frame, file_info.num_frames - 1);
    }
}
