use std::sync::atomic::Ordering;
use std::time::Duration;

use fastcord_video::DecodedFrame;
use fastcord_video::frame::{ColorDescription, FrameFormat, FramePool, Nv12};

use super::*;

/// Memory a tile holds beyond the decoder's own state: the bytes waiting for
/// the decoder plus its NV12 pictures of `width` x `height`.
pub const fn tile_bytes(width: usize, height: usize) -> usize {
    crate::gateway::MAX_QUEUED_VIDEO_BYTES
        + fastcord_video::frame::MAX_FRAME_BUFFERS * (width * height * 3 / 2)
}

/// The most a tile can hold whatever the picture size: the queue and one
/// frame pool at its ceiling.
pub const fn tile_ceiling_bytes() -> usize {
    crate::gateway::MAX_QUEUED_VIDEO_BYTES + fastcord_video::frame::MAX_FRAME_POOL_BYTES
}

fn stream(n: u64) -> TileId {
    TileId::Stream(format!("guild:1:2:{n}"))
}

fn camera(n: u64) -> TileId {
    TileId::Camera(Snowflake(n))
}

fn open(mode: Mode) -> CallView {
    CallView {
        open: true,
        mode,
        ..CallView::default()
    }
}

fn names(plan: &Plan) -> Vec<TileId> {
    plan.shown.iter().map(|(id, _)| id.clone()).collect()
}

#[test]
fn the_compact_strip_shows_cameras_small_and_leaves_streams_for_the_call_view() {
    let view = CallView::default();
    let plan = view.plan(&[stream(1), camera(5), TileId::Preview], &[]);
    assert_eq!(names(&plan), [camera(5), TileId::Preview]);
    assert!(plan.shown.iter().all(|(_, size)| *size == TileSize::Small));
    assert_eq!(plan.main, None);
    assert_eq!(plan.refused, 0);
    assert_eq!(plan.small_cameras(), [Snowflake(5)]);
}

#[test]
fn a_grid_is_large_up_to_four_tiles_then_small_and_never_over_the_cap() {
    let view = open(Mode::Grid);
    let four: Vec<_> = (1..=4).map(camera).collect();
    let plan = view.plan(&four, &[]);
    assert!(plan.shown.iter().all(|(_, size)| *size == TileSize::Large));
    assert!(
        plan.small_cameras().is_empty(),
        "large tiles want the best layer"
    );
    let five: Vec<_> = (1..=5).map(camera).collect();
    let plan = view.plan(&five, &[]);
    assert!(plan.shown.iter().all(|(_, size)| *size == TileSize::Small));
    assert_eq!(plan.small_cameras().len(), 5);

    // 4 streams + 6 cameras + the self-view: eleven videos, eight drawn, the
    // rest refused out loud rather than dropped silently.
    let mut all: Vec<_> = (1..=4).map(stream).collect();
    all.extend((1..=6).map(camera));
    all.push(TileId::Preview);
    let plan = view.plan(&all, &[]);
    assert_eq!(plan.shown.len(), MAX_VISIBLE_TILES);
    assert_eq!(plan.refused, 3);
    let refusal = plan.refusal().unwrap();
    assert!(refusal.contains("3 more videos are not shown") && refusal.contains("at most 8"));
    assert_eq!(
        Plan {
            refused: 1,
            ..view.plan(&[], &[])
        }
        .refusal()
        .unwrap()
        .split(" not shown")
        .next()
        .unwrap(),
        "1 more video is"
    );
    assert!(
        view.plan(&all[..MAX_VISIBLE_TILES], &[])
            .refusal()
            .is_none()
    );
}

#[test]
fn a_pinned_tile_survives_the_cap_and_becomes_the_large_one() {
    let mut view = open(Mode::Grid);
    let mut all: Vec<_> = (1..=4).map(stream).collect();
    all.extend((1..=6).map(camera));
    // The last camera would be refused in a grid; pinned, it is drawn large.
    view.toggle_pin(camera(6));
    assert_eq!(view.mode, Mode::Focus, "pinning in a grid focuses");
    let plan = view.plan(&all, &[]);
    assert_eq!(plan.main, Some(camera(6)));
    assert_eq!(plan.shown[0], (camera(6), TileSize::Large));
    assert_eq!(plan.shown.len(), MAX_VISIBLE_TILES);
    assert_eq!(plan.refused, 2);
    assert!(
        plan.shown[1..]
            .iter()
            .all(|(_, size)| *size == TileSize::Small)
    );
    // Unpinning returns to the default focus (the first video), mode unchanged.
    view.toggle_pin(camera(6));
    assert_eq!(view.plan(&all, &[]).main, Some(stream(1)));
    // A pin on a video that left is ignored and then forgotten.
    view.toggle_pin(camera(9));
    assert_eq!(view.plan(&all, &[]).main, Some(stream(1)));
    view.prune(&all);
    assert_eq!(view.pinned, None);
}

#[test]
fn speaker_mode_follows_the_voice_on_camera_and_keeps_them_through_silence() {
    let view = open(Mode::Speaker);
    let all = [stream(1), camera(5), camera(6)];
    // Nobody has spoken: the first video.
    assert_eq!(view.plan(&all, &[]).main, Some(stream(1)));
    assert_eq!(view.plan(&all, &[Snowflake(6)]).main, Some(camera(6)));
    // Silence keeps the last speaker large instead of flapping back.
    assert_eq!(view.plan(&all, &[]).main, Some(camera(6)));
    // A speaker without a camera changes nothing; another one with a camera does.
    assert_eq!(view.plan(&all, &[Snowflake(77)]).main, Some(camera(6)));
    assert_eq!(view.plan(&all, &[Snowflake(5)]).main, Some(camera(5)));
    // Their camera goes away: back to the first video.
    assert_eq!(
        view.plan(&[stream(1), camera(6)], &[]).main,
        Some(stream(1))
    );
    // A pin beats the speaker.
    let mut pinned = open(Mode::Speaker);
    pinned.pinned = Some(stream(1));
    assert_eq!(pinned.plan(&all, &[Snowflake(5)]).main, Some(stream(1)));
}

#[test]
fn focus_draws_the_main_tile_large_and_the_rest_small() {
    let view = open(Mode::Focus);
    let all = [stream(1), camera(5), camera(6), TileId::Preview];
    let plan = view.plan(&all, &[]);
    assert_eq!(plan.main, Some(stream(1)));
    assert_eq!(plan.shown[0].1, TileSize::Large);
    assert_eq!(
        plan.small_cameras(),
        [Snowflake(5), Snowflake(6)],
        "the strip asks for small layers; the stream and the self-view are not cameras to request"
    );
    assert_eq!(plan.refused, 0);
}

#[test]
fn a_full_view_draws_one_video_and_costs_nothing_for_the_rest() {
    let mut view = open(Mode::Grid);
    view.full = Some(camera(5));
    let all = [stream(1), camera(5), camera(6)];
    let plan = view.plan(&all, &[]);
    assert_eq!(plan.shown, [(camera(5), TileSize::Large)]);
    assert_eq!(plan.refused, 0, "hidden by choice, not refused");
    assert!(plan.small_cameras().is_empty());
    // The video ended: the full view is forgotten and the layout returns.
    view.prune(&[stream(1), camera(6)]);
    assert_eq!(view.full, None);
    assert_eq!(view.plan(&[stream(1), camera(6)], &[]).shown.len(), 2);
}

#[test]
fn the_call_view_closes_when_no_video_is_left() {
    let mut view = open(Mode::Grid);
    view.prune(&[camera(1)]);
    assert!(view.open);
    view.prune(&[]);
    assert!(!view.open);
}

#[test]
fn tile_budget_is_arithmetic_on_the_real_caps() {
    const MIB: usize = 1 << 20;
    // 720p: the 6 MiB decode queue and three 1.32 MiB pictures.
    let typical = tile_bytes(1280, 720);
    assert_eq!(typical, 6 * MIB + 3 * 1280 * 720 * 3 / 2);
    assert!(typical < 10 * MIB);
    assert!(MAX_VISIBLE_TILES * typical < 80 * MIB, "eight 720p tiles");
    // Whatever the picture size, one tile holds at most the queue and one pool.
    assert_eq!(tile_ceiling_bytes(), 6 * MIB + 48 * MIB);
    assert!(MAX_VISIBLE_TILES * tile_ceiling_bytes() <= 432 * MIB);
    // 1080p is what an official sharer may send.
    assert!(tile_bytes(1920, 1080) < 15 * MIB);
}

fn picture(pool: &mut FramePool) -> fastcord_video::frame::FrameLease {
    pool.lease(DecodedFrame {
        image: Nv12::packed(2, 2, &[7; 6]).unwrap(),
        timestamp: Duration::ZERO,
        color: ColorDescription::default(),
    })
    .unwrap()
}

/// Eleven videos (the most a call can hold), the window resized, minimized,
/// covered and restored twenty times, and every video ended: at no point more
/// than eight are visible, hidden ones hold no picture, and the end leaves
/// nothing alive.
#[test]
fn resource_pressure_resize_minimize_and_end_cycles_keep_hidden_tiles_free() {
    let mut sessions = Vec::new();
    let mut all = Vec::new();
    for n in 1..=MAX_WATCHED_STREAMS as u64 {
        let session = Session::new(format!("guild:1:2:{n}"));
        all.push(TileId::Stream(session.key.clone()));
        sessions.push(session);
    }
    for n in 1..=6u64 {
        sessions.push(Session::new(format!("camera:{n}")));
        all.push(camera(n));
    }
    sessions.push(Session::new("camera:preview".into()));
    all.push(TileId::Preview);
    assert_eq!(sessions.len(), 11);
    // Every decoder owns its pool, as in the app.
    let mut pools: Vec<_> = sessions
        .iter()
        .map(|_| FramePool::new(FrameFormat::Nv12))
        .collect();
    let mut view = open(Mode::Grid);
    let modes = [Mode::Grid, Mode::Focus, Mode::Speaker];
    for cycle in 0..20 {
        view.mode = modes[cycle % 3];
        view.open = cycle % 4 != 3;
        // 0: window up; 1: minimized; 2: covered; 3: restored.
        let on_screen = cycle % 5 != 1 && cycle % 5 != 2;
        let plan = view.plan(&all, &[Snowflake(1 + (cycle as u64 % 6))]);
        for ((session, id), pool) in sessions.iter().zip(&all).zip(&mut pools) {
            session.set_visible(on_screen && plan.contains(id));
            session.push(picture(pool));
        }
        let visible = sessions
            .iter()
            .filter(|session| session.visible.load(Ordering::Acquire))
            .count();
        assert!(visible <= MAX_VISIBLE_TILES, "cycle {cycle}: {visible}");
        if !on_screen {
            assert_eq!(visible, 0, "a minimized window decodes nothing");
        }
        for session in &sessions {
            assert_eq!(
                session.frame().is_some(),
                session.visible.load(Ordering::Acquire),
                "a hidden tile keeps no picture"
            );
        }
    }
    for session in &sessions {
        session.close();
        assert!(!session.active.load(Ordering::Acquire));
        assert!(session.frame().is_none());
    }
}

/// The UI-side cost of N drawn 720p tiles: each decoded picture is copied once
/// into its tile's pool lease and replaces the previous one. Decoding itself
/// is hardware or dav1d work on the tiles' own threads and is not measured
/// here. `cargo test --release -p fastcord-app call_layout::tests::measure -- --ignored --nocapture`
#[test]
#[ignore = "measurement, run by hand in release mode"]
fn measure_tile_hand_off_cost() {
    use std::time::Instant;
    let (width, height) = (1280u32, 720u32);
    let planes = vec![128u8; (width * height * 3 / 2) as usize];
    for tiles in [1usize, 4, 8] {
        let sessions: Vec<_> = (0..tiles).map(|n| Session::new(format!("t{n}"))).collect();
        let mut pools: Vec<_> = sessions
            .iter()
            .map(|_| FramePool::new(FrameFormat::Nv12))
            .collect();
        for session in &sessions {
            session.set_visible(true);
        }
        let seconds = 10usize;
        let frames = 30 * seconds;
        let started = Instant::now();
        for _ in 0..frames {
            for (session, pool) in sessions.iter().zip(&mut pools) {
                let lease = pool
                    .lease(DecodedFrame {
                        image: Nv12::packed(width, height, &planes).unwrap(),
                        timestamp: Duration::ZERO,
                        color: ColorDescription::default(),
                    })
                    .unwrap();
                session.push(lease);
            }
        }
        let elapsed = started.elapsed();
        let pooled: usize = pools.iter().map(FramePool::capacity_bytes).sum();
        let per_second = elapsed.as_secs_f64() / seconds as f64;
        eprintln!(
            "{tiles} tiles: {:.2} ms per picture, {:.1} % of one core at 30 fps, pools {:.1} MiB",
            elapsed.as_secs_f64() * 1000.0 / (frames * tiles) as f64,
            per_second * 100.0,
            pooled as f64 / f64::from(1 << 20),
        );
    }
}
