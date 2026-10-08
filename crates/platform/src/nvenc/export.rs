//! NVENC as an Export encoder: [`factory`] is registered in front of the software H.264 encoder
//! (`filmcraft_export::register_encoder`).
//!
//! - H.264: it takes an export only when Export ▸ Hardware encoding is Auto and NVENC can do what
//!   the settings ask; otherwise it returns `None` and the software encoder runs, exactly as before.
//! - H.265 (HEVC Main, 8-bit 4:2:0, SDR BT.709, progressive): there is no software encoder, so the
//!   Hardware encoding toggle does not apply (as with VideoToolbox) and choosing the format is the
//!   opt-in. What NVENC cannot do (HDR, interlaced, two-pass, non-square pixels, sizes outside its
//!   limits...) is counted as declined and ends the export with an error that says why; when this
//!   machine has no HEVC encoder at all, [`filmcraft_export::available`] says so and the factory
//!   returns `None` (the export's own "encoder not available" error).
//!
//! A hardware encoder that fails in the middle of an export (a lost device) ends the export with an
//! error naming it; unlike a decoder it cannot be replayed into the software encoder, because the
//! two write different streams.

use filmcraft_export::{
    BitrateMode, EncodedPacket, EncoderFrame, ExportError, ExportSettings, FieldOrder, Format, H264Pass, H264Profile, HardwareEncoding, Result, VideoEncoder,
};
use filmcraft_isobmff::{AvcConfig, SampleEntry};
use filmcraft_time::FrameRate;

use super::{Codec, Config, Nvenc, Profile};

/// The NVENC export encoder.
struct NvencEncoder {
    enc: Nvenc,
    w: u32,
    h: u32,
    rate: FrameRate,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl NvencEncoder {
    fn packets(&self, ps: Vec<super::Packet>) -> Vec<EncodedPacket> {
        let den = self.rate.den;
        let duration = u32::try_from(den).unwrap_or(u32::MAX);
        ps.into_iter().map(|p| EncodedPacket { data: p.data, key: p.key, duration, composition_offset: composition_offset(p.pts, p.dts, den) }).collect()
    }
}

/// `(pts - dts) × den` as an MP4 composition offset. It is a few frames (the B-frame delay); a
/// hostile value saturates instead of wrapping.
fn composition_offset(pts: i64, dts: i64, den: i64) -> i32 {
    let offset = pts.saturating_sub(dts).saturating_mul(den);
    i32::try_from(offset).unwrap_or(if offset < 0 { i32::MIN } else { i32::MAX })
}

impl VideoEncoder for NvencEncoder {
    fn sample_entry(&self) -> SampleEntry {
        let (sps, pps) = self.enc.parameter_sets();
        // `config` declines sizes above u16::MAX
        let (w, h) = (u16::try_from(self.w).unwrap_or(u16::MAX), u16::try_from(self.h).unwrap_or(u16::MAX));
        match self.enc.codec() {
            Codec::H264 => SampleEntry::avc(AvcConfig::new(vec![sps.to_vec()], vec![pps.to_vec()], 4), w, h),
            // `Nvenc::new` built and validated the record, so there is always one for HEVC
            Codec::Hevc => SampleEntry::hevc(self.enc.hevc_config().cloned().unwrap_or_default(), w, h),
        }
    }

    fn timescale(&self) -> u32 {
        u32::try_from(self.rate.num).unwrap_or(u32::MAX)
    }

    fn encode(&mut self, f: &EncoderFrame) -> Result<Vec<EncodedPacket>> {
        if f.hdr.is_some() {
            return Err(ExportError::Unsupported("NVENC does not take HDR pictures here".into()));
        }
        if f.width != self.w || f.height != self.h {
            return Err(ExportError::Encode(format!("NVENC: a {}x{} picture for a {}x{} encoder", f.width, f.height, self.w, self.h)));
        }
        filmcraft_export::rgba_to_yuv420_8(f.rgba, self.w as usize, self.h as usize, &mut self.y, &mut self.u, &mut self.v);
        let ps = self
            .enc
            .encode(&self.y, &self.u, &self.v, f.index)
            .map_err(|e| ExportError::Encode(format!("NVENC: {e} (turn Export ▸ Hardware encoding off to use the software encoder)")))?;
        filmcraft_export::note_hw_encode_frame();
        Ok(self.packets(ps))
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        let ps = self.enc.flush().map_err(|e| ExportError::Encode(format!("NVENC: {e}")))?;
        Ok(self.packets(ps))
    }

    fn media_start(&self) -> Option<i64> {
        // with B-frames the first DTS is `delay` frames before the first PTS
        (self.enc.delay() > 0).then_some(i64::from(self.enc.delay()).saturating_mul(self.rate.den))
    }
}

/// Why NVENC does not take this export (the software encoder does, for H.264), or the encoder's
/// configuration.
fn config(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> std::result::Result<Config, String> {
    let hevc = format == Format::Hevc;
    if !(format == Format::H264 || hevc) || s.format.is_mxf() {
        return Err("only MP4 / MOV H.264 and H.265 exports".into());
    }
    if s.signal.is_hdr() {
        return Err(if hevc {
            "HDR (hardware H.265 here is 8-bit SDR Main)".into()
        } else {
            "HDR (8-bit H.264 here carries it with software signalling)".into()
        });
    }
    if !matches!(s.h264_pass, H264Pass::Single) || (hevc && s.bitrate_mode == BitrateMode::Vbr2Pass) {
        return Err("two-pass VBR".into());
    }
    // the hardware path writes the aspect ratio of H.264 only
    if hevc && s.pixel_aspect.is_some_and(|(n, d)| n != d) {
        return Err("non-square pixels".into());
    }
    if s.field_order != FieldOrder::Progressive {
        return Err("interlaced output".into());
    }
    if w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
        return Err(format!("{w}x{h} does not fit an MP4 sample entry"));
    }
    if rate.num <= 0 || rate.den <= 0 || rate.num > i64::from(u32::MAX) || rate.den > i64::from(u32::MAX) {
        return Err("frame rate".into());
    }
    let (Ok(num), Ok(den)) = (u32::try_from(rate.num), u32::try_from(rate.den)) else {
        return Err("frame rate".into());
    };
    let fps = (num, den);
    let kbps = s.bitrate_kbps.max(100);
    Ok(Config {
        width: w,
        height: h,
        fps,
        bitrate_kbps: kbps,
        max_bitrate_kbps: s.max_bitrate_kbps.filter(|m| *m >= kbps).unwrap_or(kbps / 2 * 3),
        cbr: s.bitrate_mode == BitrateMode::Cbr,
        keyint: s.keyframe_distance.filter(|k| *k > 0).unwrap_or_else(|| (f64::from(fps.0) / f64::from(fps.1) * 2.0).round().max(1.0) as u32),
        profile: match (hevc, s.h264_profile) {
            (true, _) => Profile::HevcMain,
            (false, H264Profile::Baseline) => Profile::Baseline,
            (false, H264Profile::Main) => Profile::Main,
            (false, H264Profile::High) => Profile::High,
        },
        // the H.264 level setting means nothing for H.265: the encoder picks the level
        level: if hevc { None } else { s.h264_level },
        sar: s.pixel_aspect,
        bframes: true,
    })
}

/// The Export encoder factory (see the module documentation).
pub fn factory(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    if format == Format::Hevc {
        return hevc_factory(w, h, rate, s);
    }
    if s.hardware_encoding != HardwareEncoding::Auto || format != Format::H264 {
        return None;
    }
    let declined = |why: &str| {
        log::info!("hardware encoding declined: {why}");
        filmcraft_export::note_hw_encode_declined();
        None
    };
    let cfg = match config(format, w, h, rate, s) {
        Ok(c) => c,
        Err(why) => return declined(&why),
    };
    match Nvenc::new(&cfg) {
        Ok(enc) => {
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(NvencEncoder { enc, w, h, rate, y: Vec::new(), u: Vec::new(), v: Vec::new() })))
        }
        Err(why) => declined(&why),
    }
}

/// The H.265 side of [`factory`]: FilmCraft's only HEVC encoder on Windows, so the Hardware encoding toggle
/// does not apply and a request NVENC cannot take is an error, not a fall-through.
fn hevc_factory(w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    let declined = |why: &str| {
        log::info!("hardware H.265 encoding declined: {why}");
        filmcraft_export::note_hw_encode_declined();
        Some(Err(ExportError::Unsupported(format!("H.265 export with NVENC: {why}"))))
    };
    let cfg = match config(Format::Hevc, w, h, rate, s) {
        Ok(c) => c,
        Err(why) => return declined(&why),
    };
    match Nvenc::new(&cfg) {
        Ok(enc) => {
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(NvencEncoder { enc, w, h, rate, y: Vec::new(), u: Vec::new(), v: Vec::new() })))
        }
        // no HEVC encoder here at all: not a hardware attempt, the export's own "encoder not available" error
        Err(why) if !super::hevc_available() => {
            log::info!("no hardware H.265 encoder: {why}");
            None
        }
        Err(why) => declined(&why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_offsets_saturate() {
        assert_eq!(composition_offset(3, 1, 1001), 2002);
        assert_eq!(composition_offset(0, -2, 1001), 2002);
        assert_eq!(composition_offset(i64::MAX, i64::MIN, 1001), i32::MAX);
        assert_eq!(composition_offset(i64::MIN, i64::MAX, 1001), i32::MIN);
        assert_eq!(composition_offset(1 << 40, 0, 1), i32::MAX);
    }

    #[test]
    fn hevc_takes_what_nvenc_can_write_and_declines_the_rest() {
        let ok = ExportSettings { format: Format::Hevc, bitrate_kbps: 4000, keyframe_distance: Some(48), ..Default::default() };
        let c = config(Format::Hevc, 1280, 720, FrameRate::FPS_24, &ok).unwrap();
        assert_eq!((c.profile, c.level, c.keyint, c.fps), (Profile::HevcMain, None, 48, (24, 1)));
        // the H.264 level and profile settings mean nothing to HEVC
        let h = ExportSettings { h264_level: Some(41), h264_profile: H264Profile::Baseline, ..ok.clone() };
        assert_eq!(config(Format::Hevc, 1280, 720, FrameRate::FPS_24, &h).unwrap().level, None);
        // the Hardware encoding toggle does not matter for HEVC (it has no software encoder)
        let off = ExportSettings { hardware_encoding: HardwareEncoding::Off, ..ok.clone() };
        assert!(config(Format::Hevc, 1280, 720, FrameRate::FPS_24, &off).is_ok());
        for (what, s) in [
            ("HDR", ExportSettings { signal: filmcraft_export::ColorSignal::PQ, ..ok.clone() }),
            ("two-pass", ExportSettings { bitrate_mode: BitrateMode::Vbr2Pass, ..ok.clone() }),
            ("analysis pass", ExportSettings { h264_pass: H264Pass::First, ..ok.clone() }),
            ("interlaced", ExportSettings { field_order: FieldOrder::UpperFirst, ..ok.clone() }),
            ("non-square pixels", ExportSettings { pixel_aspect: Some((4, 3)), ..ok.clone() }),
            ("MXF", ExportSettings { format: Format::MxfOp1a, ..ok.clone() }),
        ] {
            assert!(config(Format::Hevc, 1280, 720, FrameRate::FPS_24, &s).is_err(), "{what}");
        }
        // hostile sizes and frame rates
        assert!(config(Format::Hevc, 70_000, 64, FrameRate::FPS_24, &ok).is_err());
        assert!(config(Format::Hevc, 64, 70_000, FrameRate::FPS_24, &ok).is_err());
        for rate in [FrameRate { num: 0, den: 1 }, FrameRate { num: 24, den: 0 }, FrameRate { num: -24, den: 1 }, FrameRate { num: i64::MAX, den: 1 }] {
            assert!(config(Format::Hevc, 1280, 720, rate, &ok).is_err(), "{rate:?}");
        }
        // other formats are not NVENC's
        assert!(config(Format::ProRes, 1280, 720, FrameRate::FPS_24, &ok).is_err());
    }

    #[test]
    fn oversized_pictures_are_declined() {
        let s = ExportSettings { format: Format::H264, hardware_encoding: HardwareEncoding::Auto, ..Default::default() };
        assert!(config(Format::H264, 70_000, 64, FrameRate::FPS_24, &s).is_err());
        assert!(config(Format::H264, 64, 70_000, FrameRate::FPS_24, &s).is_err());
    }
}
