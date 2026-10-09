//! Hardware decoding through VA-API (Linux: Intel, AMD and other Mesa / vendor drivers).
//!
//! VA-API decoding is stateless: the application parses the stream and keeps the decoded picture
//! buffer, and the GPU decodes one picture's slice data at a time into a surface. [`h264`] does the
//! host side with the software decoder's own parsers and DPB (so the two decide alike), [`va`]
//! drives libva (loaded at run time; no libva means no hardware decoder, never a failure to start),
//! and [`VaDecoder`] puts them together as a [`VideoDecoder`]. H.264 only so far: 8-bit 4:2:0
//! progressive, Constrained Baseline / Main / High; everything else stays in software.
//!
//! The FFI is in [`ffi`] (declarations) and `va` (calls); the rest of this module is safe code.

pub mod ffi;
pub mod h264;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod va;

#[cfg(test)]
mod abi_tests;
#[cfg(test)]
mod tests;

#[cfg(target_os = "linux")]
pub use linux::VaDecoder;

#[cfg(target_os = "linux")]
mod linux {
    use filmcraft_bitstream::unescape_rbsp;
    use filmcraft_codecs::hw::{NalCodec, NalStreamInfo};
    use filmcraft_codecs::{CodecError, DecodedFrame, Result, VideoDecoder};
    use filmcraft_h264::params::Sps;

    use super::ffi;
    use super::h264::Front;
    use super::va::Session;
    use crate::biplanar::Geometry;

    /// The VA-API profiles that decode an H.264 stream of `profile_idc`, best fit first (a Main or
    /// High decoder also decodes what the smaller profiles allow).
    pub(crate) fn profiles(profile_idc: u8) -> Option<&'static [ffi::VAProfile]> {
        Some(match profile_idc {
            66 => &[ffi::VAProfileH264ConstrainedBaseline, ffi::VAProfileH264Main, ffi::VAProfileH264High],
            77 => &[ffi::VAProfileH264Main, ffi::VAProfileH264High],
            100 => &[ffi::VAProfileH264High],
            _ => return None,
        })
    }

    /// A VA-API H.264 decoder for one stream.
    pub struct VaDecoder {
        front: Front<Session>,
        info: NalStreamInfo,
        name: String,
        fed: u64,
        fail_after: Option<u64>,
    }

    impl VaDecoder {
        /// A decoder for the stream `info` describes, or why VA-API does not take it on this system.
        pub fn new(info: NalStreamInfo) -> std::result::Result<Self, String> {
            if info.codec != NalCodec::H264 {
                return Err("only H.264 is decoded through VA-API so far".into());
            }
            if info.chroma_format_idc != 1 || info.bit_depth_luma != 8 || info.bit_depth_chroma != 8 || info.interlaced {
                return Err(format!(
                    "{} 4:2:0 {}-bit {} is not taken",
                    if info.interlaced { "interlaced" } else { "progressive" },
                    info.bit_depth_luma,
                    if info.chroma_format_idc == 1 { "only" } else { "(other chroma formats)" }
                ));
            }
            let profiles = profiles(info.profile_idc).ok_or_else(|| format!("H.264 profile_idc {} is not taken", info.profile_idc))?;
            let sps = info
                .parameter_sets
                .iter()
                .find(|n| n.first().is_some_and(|h| h & 0x1f == 7))
                .and_then(|n| Sps::parse(&unescape_rbsp(n.get(1..)?)).ok())
                .ok_or("no readable SPS")?;
            sps.check_supported().map_err(|e| e.to_string())?;
            let mbs = (sps.pic_width_in_mbs, sps.frame_height_in_mbs());
            // the DPB, the picture being decoded, and one spare
            let count = sps.max_dpb_frames().saturating_add(2).min(18);
            let geometry = Geometry { crop: info.crop, bits: 8, color: info.color, par: info.par };
            let session = Session::new(profiles, (sps.width(), sps.height()), count, geometry)?;
            let name = format!("VA-API H.264 ({})", session.vendor());
            let front = Front::new(session, info.length_size, mbs, &info.parameter_sets).map_err(|e| e.to_string())?;
            Ok(Self { front, info, name, fed: 0, fail_after: None })
        }

        /// Test hook: fail every decode call after the first `n` (a GPU lost mid-stream).
        pub fn fail_after(&mut self, n: u64) {
            self.fail_after = Some(n);
        }
    }

    impl VideoDecoder for VaDecoder {
        fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
            self.fed += 1;
            if self.fail_after.is_some_and(|n| self.fed > n) {
                return Err(CodecError::Decode("VA-API decoder failed (test hook)".into()));
            }
            self.front.decode(sample, pts)
        }

        fn flush(&mut self) -> Vec<DecodedFrame> {
            match self.front.flush() {
                Ok(out) => out,
                Err(e) => {
                    log::warn!("{}: {e} while flushing", self.name);
                    Vec::new()
                }
            }
        }

        fn reset(&mut self) {
            self.front.reset();
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn is_random_access(&self, sample: &[u8]) -> Option<bool> {
            self.info.is_random_access(sample)
        }

        fn is_disposable(&self, sample: &[u8]) -> bool {
            self.info.is_disposable(sample)
        }
    }
}
