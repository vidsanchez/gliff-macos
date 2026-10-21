#![cfg(target_os = "macos")]
use gliff_sw::Decoder;

fn fixture(w: u32, h: u32, frame: usize) -> &'static [u8] {
    match (w, h, frame) {
        (64, 48, 0) => include_bytes!("fixtures/64x48-0.h264"),
        (64, 48, 1) => include_bytes!("fixtures/64x48-1.h264"),
        (64, 48, 2) => include_bytes!("fixtures/64x48-2.h264"),
        (80, 64, 0) => include_bytes!("fixtures/80x64-0.h264"),
        (34, 18, 0) => include_bytes!("fixtures/34x18-0.h264"),
        (16, 16, 0) => include_bytes!("fixtures/16x16-0.h264"),
        (128, 96, 0) => include_bytes!("fixtures/128x96-0.h264"),
        (80, 64, 1) => include_bytes!("fixtures/80x64-1.h264"),
        (80, 64, 2) => include_bytes!("fixtures/80x64-2.h264"),
        (34, 18, 1) => include_bytes!("fixtures/34x18-1.h264"),
        (34, 18, 2) => include_bytes!("fixtures/34x18-2.h264"),
        (16, 16, 1) => include_bytes!("fixtures/16x16-1.h264"),
        (16, 16, 2) => include_bytes!("fixtures/16x16-2.h264"),
        (128, 96, 1) => include_bytes!("fixtures/128x96-1.h264"),
        (128, 96, 2) => include_bytes!("fixtures/128x96-2.h264"),
        _ => panic!("unknown fixture"),
    }
}
#[test]
fn native_single420_is_synchronous_and_preserves_bgra_colors() {
    let mut decoder = Decoder::new(false, 64, 48).unwrap();
    for (n, (y, u, v)) in [(63, 102, 240), (32, 240, 118), (173, 42, 26)]
        .iter()
        .copied()
        .enumerate()
    {
        let expected = gliff_sw::convert::i420_to_bgra(&[y; 4], &[u], &[v], (2, 1, 1), 2, 2);
        let frame = decoder
            .decode(fixture(64, 48, n), &[])
            .unwrap()
            .expect("one output per AU");
        assert_eq!(
            (frame.width, frame.height, frame.pixels.len()),
            (64, 48, 64 * 48 * 4)
        );
        let middle = &frame.pixels[(24 * 64 + 32) * 4..][..4];
        for channel in 0..3 {
            assert!(
                middle[channel].abs_diff(expected[channel]) <= 12,
                "{middle:?}"
            );
        }
        assert!(!decoder.has_pending());
        assert!(decoder.flush().unwrap().is_none());
    }
}
#[test]
fn cropped_macroblocks_small_frames_and_drop_recreate() {
    for _ in 0..8 {
        for (w, h) in [(64, 48), (80, 64), (34, 18), (16, 16), (128, 96)] {
            let mut decoder = Decoder::new(false, w, h).unwrap();
            for n in 0..3 {
                let frame = decoder.decode(fixture(w, h, n), &[]).unwrap().unwrap();
                assert_eq!(
                    (frame.width, frame.height, frame.pixels.len()),
                    (w, h, (w * h * 4) as usize)
                );
            }
        }
    }
}
#[test]
fn missing_corrupt_parameters_auxiliary_and_geometry_fail_closed_then_recover() {
    assert!(Decoder::new(true, 64, 48).is_err());
    assert!(Decoder::new(false, 4098, 2).is_err());
    let mut decoder = Decoder::new(false, 64, 48).unwrap();
    assert!(decoder.decode(fixture(64, 48, 1), &[]).is_err());
    assert!(decoder.decode(&[0, 0, 1, 0x65], &[]).is_err());
    assert!(decoder.decode(fixture(64, 48, 0), &[1]).is_err());
    assert!(decoder.decode(fixture(80, 64, 0), &[]).is_err());
    assert!(decoder.decode(fixture(64, 48, 0), &[]).unwrap().is_some());
    let bytes = fixture(64, 48, 1);
    assert!(decoder.decode(&bytes[..5], &[]).is_err());
    assert!(decoder.decode(fixture(64, 48, 0), &[]).unwrap().is_some());
    // Repeat parameter sets and an IDR without replacing an identical session.
    assert!(decoder.decode(fixture(64, 48, 0), &[]).unwrap().is_some());
}

#[test]
fn same_geometry_parameter_update_requires_idr_and_recreates_session() {
    let mut decoder = Decoder::new(false, 64, 48).unwrap();
    decoder.decode(fixture(64, 48, 0), &[]).unwrap();
    let mut updated = fixture(64, 48, 0).to_vec();
    // Byte 7 is SPS level_idc after the four-byte prefix, NAL, profile, flags.
    assert_eq!(updated[4] & 31, 7);
    updated[7] = 31;
    let end = updated[4..]
        .windows(4)
        .position(|b| b == [0, 0, 0, 1])
        .unwrap()
        + 4;
    let mut changed_then_p = updated[..end].to_vec();
    changed_then_p.extend_from_slice(fixture(64, 48, 1));
    assert!(decoder.decode(&changed_then_p, &[]).is_err());
    assert!(decoder.decode(&updated, &[]).unwrap().is_some());
    assert!(decoder.decode(fixture(64, 48, 1), &[]).unwrap().is_some());
}
