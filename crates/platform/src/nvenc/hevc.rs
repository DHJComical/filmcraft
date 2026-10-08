//! What is specific to HEVC in the NVENC backend (safe code): the level codes, and the `hvcC`
//! record built from the parameter sets the encoder wrote.
//!
//! Nothing in the record is invented: the profile, tier, compatibility flags, level, chroma format,
//! bit depths and temporal layers come from the SPS (parsed with `filmcraft_hevc`), and the 48
//! general constraint flags and `sps_temporal_id_nesting_flag` are read from the SPS bits that carry
//! them (`parse_ptl` skips the flags and `Sps` does not keep the nesting flag). The array
//! completeness flags are 1 and the parameter sets are only in the record, never in the samples
//! (the `hvc1` sample entry).

use filmcraft_bitstream::unescape_rbsp;
use filmcraft_hevc::params::Sps;
use filmcraft_isobmff::{HevcConfig, HevcNalArray};

use super::{Codec, nal_type};

/// `NV_ENC_LEVEL_HEVC_*` for a level × 10 (4.1 is 41): HEVC levels are numbered `level × 30`.
/// `None` for a number that is not an HEVC level.
pub fn level_code(level_x10: u8) -> Option<u8> {
    matches!(level_x10, 10 | 20 | 21 | 30 | 31 | 40 | 41 | 50 | 51 | 52 | 60 | 61 | 62).then(|| level_x10.saturating_mul(3))
}

/// Bytes of the SPS RBSP before the general profile / tier / level record: `sps_video_parameter_set_id`,
/// `sps_max_sub_layers_minus1` and `sps_temporal_id_nesting_flag` share the first one.
const PTL_START: usize = 1;
/// General profile space / tier / profile (1 byte), compatibility flags (4), constraint flags (6), level (1).
const PTL_LEN: usize = 12;

/// The `hvcC` record of a stream of `size` pictures (width, height) whose parameter sets are `vps`,
/// `sps` and `pps` (NAL units with their headers, without start codes or length prefixes). An error
/// says why the stream is not one this backend writes: HEVC Main, 8-bit 4:2:0.
pub fn hevc_config(vps: &[u8], sps: &[u8], pps: &[u8], size: (u32, u32)) -> Result<HevcConfig, String> {
    for (nal, kind, name) in [(vps, 32u8, "VPS"), (sps, 33, "SPS"), (pps, 34, "PPS")] {
        if nal_type(Codec::Hevc, nal) != Some(kind) || nal.len() <= 2 {
            return Err(format!("the encoder's {name} is not a {name} NAL unit"));
        }
        // the record stores each length in 16 bits
        if u16::try_from(nal.len()).is_err() {
            return Err(format!("the {name} is too long for an hvcC record"));
        }
    }
    let rbsp = unescape_rbsp(sps.get(2..).unwrap_or_default());
    let parsed = Sps::parse(&rbsp).map_err(|e| format!("unreadable HEVC SPS: {e}"))?;
    let ptl_bytes = rbsp.get(PTL_START..PTL_START + PTL_LEN).ok_or("the HEVC SPS is truncated before its profile / tier / level")?;
    // bytes 5..11 of the record are the 48 general constraint indicator flags
    let constraints = ptl_bytes.get(5..11).ok_or("the HEVC SPS is truncated before its constraint flags")?;
    let general_constraint_indicator_flags = constraints.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    let temporal_id_nested = rbsp.first().is_some_and(|b| b & 1 != 0);

    let ptl = &parsed.ptl;
    if ptl.profile_space != 0 || ptl.profile_idc != 1 {
        return Err(format!("the encoder wrote HEVC profile {} (space {}), not Main", ptl.profile_idc, ptl.profile_space));
    }
    if parsed.chroma_format_idc != 1 || parsed.bit_depth_luma != 8 || parsed.bit_depth_chroma != 8 {
        return Err(format!(
            "the encoder wrote chroma format {} at {}/{} bits, not 8-bit 4:2:0",
            parsed.chroma_format_idc, parsed.bit_depth_luma, parsed.bit_depth_chroma
        ));
    }
    let (_, _, w, h) = parsed.crop_rect();
    if (w, h) != size {
        return Err(format!("the encoder wrote {w}x{h} pictures for a {}x{} export", size.0, size.1));
    }
    Ok(HevcConfig {
        general_profile_space: ptl.profile_space,
        general_tier_flag: ptl.tier,
        general_profile_idc: ptl.profile_idc,
        general_profile_compatibility_flags: ptl.compatibility,
        general_constraint_indicator_flags,
        general_level_idc: ptl.level_idc,
        // 0: not specified, which is a valid value for both
        min_spatial_segmentation_idc: 0,
        parallelism_type: 0,
        chroma_format_idc: u8::try_from(parsed.chroma_format_idc).map_err(|_| "chroma format".to_string())?,
        bit_depth_luma: u8::try_from(parsed.bit_depth_luma).map_err(|_| "bit depth".to_string())?,
        bit_depth_chroma: u8::try_from(parsed.bit_depth_chroma).map_err(|_| "bit depth".to_string())?,
        // 0: unspecified average rate, and the frame rate may not be constant
        avg_frame_rate: 0,
        constant_frame_rate: 0,
        num_temporal_layers: u8::try_from(parsed.max_sub_layers_minus1.saturating_add(1)).map_err(|_| "temporal layers".to_string())?,
        temporal_id_nested,
        length_size: 4,
        arrays: vec![
            HevcNalArray { completeness: true, nal_type: 32, nalus: vec![vps.to_vec()] },
            HevcNalArray { completeness: true, nal_type: 33, nalus: vec![sps.to_vec()] },
            HevcNalArray { completeness: true, nal_type: 34, nalus: vec![pps.to_vec()] },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parameter sets NVENC wrote for a 1280x720, 24 fps, B-frame HEVC Main stream (RTX 5060, driver
    // 617.14): the coded size is 1280x736 and a conformance window crops it to 1280x720.
    const VPS: &str = "40010c01ffff01600000030090000003000003005d9940c0000003004000000614";
    const SPS: &str = "42010101600000030090000003000003005da00280802e1f1396654a421191bff0c05a8080808a0000030002000003003010";
    const PPS: &str = "4401c1937c0cc9";

    fn unhex(h: &str) -> Vec<u8> {
        (0..h.len()).step_by(2).filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
    }

    fn record() -> HevcConfig {
        hevc_config(&unhex(VPS), &unhex(SPS), &unhex(PPS), (1280, 720)).unwrap()
    }

    #[test]
    fn hevc_levels_are_thirty_times_the_level() {
        assert_eq!(level_code(41), Some(123));
        assert_eq!(level_code(10), Some(30));
        assert_eq!(level_code(21), Some(63));
        assert_eq!(level_code(62), Some(186));
        for bad in [0, 11, 42, 53, 70, 255] {
            assert_eq!(level_code(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_record_is_the_sps_it_came_from() {
        let c = record();
        let sps = unhex(SPS);
        let rbsp = unescape_rbsp(&sps[2..]);
        // profile / tier / compatibility / constraint flags / level are the SPS's own bytes
        assert_eq!(&c.to_bytes()[1..13], &rbsp[1..13]);
        assert_eq!((c.general_profile_space, c.general_tier_flag, c.general_profile_idc), (0, false, 1));
        assert_eq!(c.general_profile_compatibility_flags, 0x6000_0000, "Main and Main 10");
        // progressive_source_flag and frame_only_constraint_flag
        assert_eq!(c.general_constraint_indicator_flags, 0x9000_0000_0000);
        assert_eq!(c.general_level_idc, 93, "level 3.1");
        assert_eq!((c.chroma_format_idc, c.bit_depth_luma, c.bit_depth_chroma), (1, 8, 8));
        assert_eq!((c.num_temporal_layers, c.temporal_id_nested, c.length_size), (1, true, 4));
        assert_eq!(c.arrays.iter().map(|a| (a.nal_type, a.completeness, a.nalus.len())).collect::<Vec<_>>(), vec![(32, true, 1), (33, true, 1), (34, true, 1)]);
        assert_eq!((c.vps(), c.sps(), c.pps()), (vec![&unhex(VPS)[..]], vec![&sps[..]], vec![&unhex(PPS)[..]]));
        assert_eq!(HevcConfig::parse(&c.to_bytes()).unwrap(), c);
    }

    #[test]
    fn the_picture_size_has_to_match_after_cropping() {
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        for wrong in [(1280, 736), (1920, 1080), (1280, 719), (0, 0)] {
            let e = hevc_config(&v, &s, &p, wrong).unwrap_err();
            assert!(e.contains("1280x720"), "{e}");
        }
    }

    #[test]
    fn a_stream_that_is_not_main_8_bit_420_is_refused() {
        // The same SPS with the general profile changed (bits of profile_idc in the first PTL byte),
        // the chroma format or the bit depth changed would be other streams: edit the PTL byte only
        // (it sits before any emulation-prevention byte).
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        let mut other_profile = s.clone();
        other_profile[3] = (other_profile[3] & !0x1f) | 2; // Main 10
        assert!(hevc_config(&v, &other_profile, &p, (1280, 720)).unwrap_err().contains("not Main"));
        let mut other_space = s.clone();
        other_space[3] |= 0x40;
        assert!(hevc_config(&v, &other_space, &p, (1280, 720)).is_err());
    }

    #[test]
    fn truncated_and_corrupt_parameter_sets_are_errors_never_panics() {
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        // every truncation of the SPS
        for n in 0..s.len() {
            assert!(hevc_config(&v, &s[..n], &p, (1280, 720)).is_err(), "SPS cut at {n}");
        }
        // a bit flip anywhere in the SPS: an error or a record, but never a panic
        for byte in 0..s.len() {
            for bit in 0..8 {
                let mut m = s.clone();
                m[byte] ^= 1 << bit;
                let r = std::panic::catch_unwind(|| hevc_config(&v, &m, &p, (1280, 720)));
                assert!(r.is_ok(), "panic with bit {bit} of byte {byte} flipped");
            }
        }
        // wrong NAL types, empty and one-byte NAL units
        let cases: [(&[u8], &[u8], &[u8]); 5] = [
            (&[], &[], &[]),
            (&[0x40], &[0x42], &[0x44]),
            (&[0x40, 0x01, 0xAA], &[0x42, 0x01], &[0x44, 0x01, 0xBB]),
            (&[0x42, 0x01, 0xAA], &s, &p),
            (&v, &p, &s),
        ];
        for (v, s, p) in cases {
            assert!(hevc_config(v, s, p, (64, 64)).is_err(), "{v:?} {s:?} {p:?}");
        }
        // a parameter set too long for the record's 16-bit lengths
        let mut huge = s.clone();
        huge.resize(70_000, 0x55);
        assert!(hevc_config(&v, &huge, &p, (1280, 720)).is_err());
    }
}
