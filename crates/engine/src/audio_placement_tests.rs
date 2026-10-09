//! Multi-clip audio placement: `audio_placement_specs` and dynamic audio track allocation.

use filmcraft_project::{AudioChannels, ItemId, TrackKind};

use serde_json::json;

use crate::Session;
use crate::commands::{AudioPlacementSpec, audio_placement_specs, ensure_audio_tracks};

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

fn active(s: &Session) -> ItemId {
    s.state.active_sequence.expect("demo session has an active sequence")
}

#[test]
fn specs_always_place_one_clip_on_the_destination() {
    assert_eq!(audio_placement_specs(&[]), vec![AudioPlacementSpec { track_offset: 0, source_channels: None }]);
    assert_eq!(audio_placement_specs(&[vec![0, 1]]).len(), 1);
}

#[test]
fn specs_put_further_clips_on_the_tracks_below() {
    let specs = audio_placement_specs(&[vec![0], vec![1], vec![2, 3]]);
    let got: Vec<(usize, Option<Vec<u16>>)> = specs.into_iter().map(|s| (s.track_offset, s.source_channels)).collect();
    assert_eq!(got, vec![(0, None), (1, Some(vec![1])), (2, Some(vec![2, 3]))]);
}

#[test]
fn specs_are_capped_for_hostile_channel_maps() {
    let clips = vec![vec![0u16]; 100_000];
    assert!(audio_placement_specs(&clips).len() <= crate::sequence_tools::MAX_TRACKS);
}

#[test]
fn missing_audio_tracks_are_cloned_from_the_destination() {
    let s = demo();
    let seq = active(&s);
    let mut p = (*s.project).clone();
    let have = p.sequence(seq).map(|q| q.audio_tracks.len()).unwrap_or(0);
    assert!(have >= 1);
    if let Some(t) = p.sequence_mut(seq).and_then(|q| q.audio_tracks.get_mut(0)) {
        t.channels = AudioChannels::Mono;
        t.volume_db = -3.0;
        t.locked = true;
    }
    let ids = ensure_audio_tracks(&mut p, seq, 0, have + 3, "test").unwrap();
    assert_eq!(ids.len(), have + 3);
    let q = p.sequence(seq).unwrap();
    for t in &q.audio_tracks[have..] {
        assert_eq!(t.kind, TrackKind::Audio);
        assert_eq!(t.channels, AudioChannels::Mono);
        assert_eq!(t.volume_db, -3.0);
        assert!(t.items.is_empty() && !t.locked);
    }
    let mut unique = ids.clone();
    unique.sort_by_key(|t| t.0);
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "track ids are unique");
    // enough tracks already: nothing is added
    let again = ensure_audio_tracks(&mut p, seq, 0, have + 3, "test").unwrap();
    assert_eq!(again, ids);
}

#[test]
fn hostile_track_counts_are_an_error_not_a_crash() {
    let s = demo();
    let seq = active(&s);
    let mut p = (*s.project).clone();
    assert!(ensure_audio_tracks(&mut p, seq, usize::MAX, usize::MAX, "test").is_err());
    assert!(ensure_audio_tracks(&mut p, seq, 0, 1_000_000, "test").is_err());
    assert!(ensure_audio_tracks(&mut p, ItemId(u64::MAX), 0, 1, "test").is_err());
}
