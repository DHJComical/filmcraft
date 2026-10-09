//! The H.264 front end against a recording stand-in for the hardware, on every system: for x264
//! streams with B-pyramids, several references, weighted prediction, several slices and open GOPs,
//! the pictures come out in the software decoder's order with its pts (whole stream, after seeks,
//! after a mid-stream flush), and every buffer handed to the hardware is consistent: the current
//! picture's surface is free, every reference is a decoded surface listed in the DPB, reference
//! lists have the active number of entries, and slice data offsets lie inside the slice.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use filmcraft_codecs::DecodedFrame;
use filmcraft_codecs::hw::NalStreamInfo;
use filmcraft_frame::VideoFrame;

use super::ffi::{self, VAIQMatrixBufferH264, VAPictureH264, VAPictureParameterBufferH264, VASliceParameterBufferH264};
use super::h264::{Accel, Front};
use crate::biplanar::{self, Biplanar, Geometry};

/// Records what it is asked to do and checks it; "decodes" nothing.
struct Recorder {
    ids: Vec<ffi::VASurfaceID>,
    decoded: Vec<bool>,
    pictures: usize,
    frame: VideoFrame,
}

impl Recorder {
    fn new(count: usize) -> Self {
        let g = Geometry { crop: (0, 0, 2, 2), bits: 8, color: Default::default(), par: (1, 1) };
        let frame = biplanar::to_frame(&Biplanar { luma: &[128; 4], chroma: &[128; 2], stride: 2, rows: 2 }, &g).unwrap();
        Self { ids: (0..count as u32).map(|i| 0x100 + i).collect(), decoded: vec![false; count], pictures: 0, frame }
    }
}

fn valid(p: &VAPictureH264) -> bool {
    p.flags & ffi::VA_PICTURE_H264_INVALID == 0
}

impl Accel for Recorder {
    fn surfaces(&self) -> &[ffi::VASurfaceID] {
        &self.ids
    }

    fn decode(
        &mut self,
        target: usize,
        pic: &VAPictureParameterBufferH264,
        _iq: &VAIQMatrixBufferH264,
        slices: &[(VASliceParameterBufferH264, Vec<u8>)],
    ) -> Result<(), String> {
        assert_eq!(pic.CurrPic.picture_id, self.ids[target], "current picture surface");
        let refs: Vec<_> = pic.ReferenceFrames.iter().filter(|r| valid(r)).collect();
        let ref_ids: BTreeSet<_> = refs.iter().map(|r| r.picture_id).collect();
        assert_eq!(ref_ids.len(), refs.len(), "each reference frame once");
        assert!(!ref_ids.contains(&pic.CurrPic.picture_id), "the target is not a reference");
        for r in &refs {
            let i = self.ids.iter().position(|&id| id == r.picture_id).expect("reference is a decoder surface");
            assert!(self.decoded[i], "reference surface {i} holds a picture");
            assert!(r.flags & (ffi::VA_PICTURE_H264_SHORT_TERM_REFERENCE | ffi::VA_PICTURE_H264_LONG_TERM_REFERENCE) != 0, "reference flags");
        }
        assert!(!slices.is_empty());
        for (s, data) in slices {
            assert_eq!(s.slice_data_size as usize, data.len());
            assert!((s.slice_data_bit_offset as usize) < data.len() * 8, "slice data starts inside the slice");
            let ty = s.slice_type;
            let lists: &[(&[VAPictureH264; 32], u8)] = match ty {
                2 => &[],
                0 => &[(&s.RefPicList0, s.num_ref_idx_l0_active_minus1)],
                _ => &[(&s.RefPicList0, s.num_ref_idx_l0_active_minus1), (&s.RefPicList1, s.num_ref_idx_l1_active_minus1)],
            };
            for (list, minus1) in lists {
                let n = *minus1 as usize + 1;
                assert!(list[..n].iter().all(valid), "{n} active entries");
                assert!(list[n..].iter().all(|p| !valid(p)), "nothing past the active entries");
                assert!(list[..n].iter().all(|p| ref_ids.contains(&p.picture_id)), "list entries are reference frames");
            }
        }
        self.decoded[target] = true;
        self.pictures += 1;
        Ok(())
    }

    fn read(&mut self, index: usize) -> Result<VideoFrame, String> {
        assert!(self.decoded[index], "read back a decoded surface");
        Ok(self.frame.clone())
    }

    fn fill(&mut self, index: usize, from: Option<usize>) -> Result<(), String> {
        assert!(from.is_none_or(|f| self.decoded[f]));
        self.decoded[index] = true;
        Ok(())
    }
}

/// (file, x264 profile, x264 parameters).
const STREAMS: &[(&str, &str, &str)] = &[
    ("va_front_high.mp4", "high", "bframes=3:b-pyramid=normal:keyint=12:min-keyint=12:scenecut=0:ref=4:weightp=2"),
    ("va_front_slices.mp4", "baseline", "keyint=12:min-keyint=12:scenecut=0:slices=4:ref=2"),
    ("va_front_open_gop.mp4", "high", "bframes=3:b-pyramid=strict:keyint=12:min-keyint=12:scenecut=0:open-gop=1:direct=temporal"),
];

fn fixture(ff: &std::path::Path, name: &str, profile: &str, params: &str) -> Option<PathBuf> {
    let out = filmcraft_testkit::fixtures_dir("platform").join(name);
    let src = "testsrc2=s=320x184:r=24:d=2";
    filmcraft_testkit::fixtures::generate(&out, |tmp| {
        Command::new(ff)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", src, "-c:v", "libx264", "-profile:v", profile, "-x264-params", params, "-pix_fmt", "yuv420p"])
            .arg(tmp)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

struct Stream {
    entry: filmcraft_isobmff::SampleEntry,
    samples: Vec<(Vec<u8>, i64)>,
    sync: Vec<bool>,
}

fn read_stream(path: &std::path::Path) -> Stream {
    let bytes = std::fs::read(path).unwrap();
    let file = filmcraft_isobmff::open(bytes.clone()).unwrap();
    let t = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
    let track = &file.tracks[t];
    let samples = (0..track.samples.len()).map(|i| (file.read_sample(&bytes, t, i).unwrap(), track.samples[i].pts)).collect();
    Stream { entry: track.entries[0].clone(), samples, sync: track.samples.iter().map(|s| s.is_sync).collect() }
}

fn front(s: &Stream) -> Front<Recorder> {
    let info = NalStreamInfo::from_entry(&s.entry).unwrap().unwrap();
    let mbs = (info.coded.0 / 16, info.coded.1 / 16);
    Front::new(Recorder::new(18), info.length_size, mbs, &info.parameter_sets).unwrap()
}

fn pts(frames: &[DecodedFrame]) -> Vec<i64> {
    frames.iter().map(|f| f.pts).collect()
}

fn run_front(f: &mut Front<Recorder>, samples: &[(Vec<u8>, i64)]) -> Vec<i64> {
    let mut out = Vec::new();
    for (s, p) in samples {
        out.extend(pts(&f.decode(s, *p).unwrap()));
    }
    out.extend(pts(&f.flush().unwrap()));
    out
}

fn run_software(d: &mut dyn filmcraft_codecs::VideoDecoder, samples: &[(Vec<u8>, i64)]) -> Vec<i64> {
    let mut out = Vec::new();
    for (s, p) in samples {
        out.extend(pts(&d.decode(s, *p).unwrap()));
    }
    out.extend(pts(&d.flush()));
    out
}

#[test]
fn output_order_and_buffers_match_the_software_decoder() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    for (name, profile, params) in STREAMS {
        let Some(path) = fixture(&ff, name, profile, params) else { continue };
        let s = read_stream(&path);
        let mut f = front(&s);
        let mut sw = filmcraft_codecs::software_video_decoder(&s.entry).unwrap();
        let a = run_front(&mut f, &s.samples);
        assert_eq!(a, run_software(sw.as_mut(), &s.samples), "{name}");
        assert_eq!(f.accel().pictures, s.samples.len(), "{name}: every picture decoded");
        // seeks to every sync sample (open-GOP I pictures included: frame_num gap stand-ins)
        for k in (1..s.samples.len()).filter(|&i| s.sync[i]) {
            f.reset();
            sw.reset();
            let end = (k + 13).min(s.samples.len());
            assert_eq!(run_front(&mut f, &s.samples[k..end]), run_software(sw.as_mut(), &s.samples[k..end]), "{name} from sample {k}");
        }
        // a flush mid-GOP, then on
        f.reset();
        sw.reset();
        let mid = s.samples.len() / 2 + 1;
        let mut a = run_front(&mut f, &s.samples[..mid]);
        a.extend(run_front(&mut f, &s.samples[mid..]));
        let mut b = run_software(sw.as_mut(), &s.samples[..mid]);
        b.extend(run_software(sw.as_mut(), &s.samples[mid..]));
        assert_eq!(a, b, "{name} with a flush at {mid}");
    }
}

/// Damaged samples give errors (the hybrid decoder then continues in software), never a panic.
#[test]
fn damaged_samples_are_errors() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let (name, profile, params) = STREAMS[0];
    let Some(path) = fixture(&ff, name, profile, params) else { return };
    let s = read_stream(&path);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..40 {
        let mut f = front(&s);
        for (i, (smp, p)) in s.samples.iter().enumerate() {
            let mut m = smp.clone();
            if i % 3 == round % 3 {
                match next() % 3 {
                    0 => m.truncate(next() as usize % m.len().max(1)),
                    1 => {
                        let at = next() as usize % m.len().max(1);
                        if let Some(b) = m.get_mut(at) {
                            *b ^= 1 << (next() % 8);
                        }
                    }
                    _ => m.iter_mut().skip(4).take(6).for_each(|b| *b = next() as u8),
                }
            }
            if f.decode(&m, *p).is_err() {
                f.reset();
            }
        }
        let _ = f.flush();
    }
}
