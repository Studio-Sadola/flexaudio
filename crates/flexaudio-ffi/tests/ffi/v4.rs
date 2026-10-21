//! Offline contract checks for v4 boundaries, reports, and independent lifecycle delivery.
use super::*;
use crate::types::FlexConfig;
use crate::{error::code, lifecycle_v4::*, settings_v4::*, v2::*, v2_storage::FlexErrorInfoV2};
use std::{
    collections::VecDeque,
    ffi::CStr,
    mem, ptr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn base() -> FlexConfig {
    unsafe { mem::zeroed() }
}
fn v3(base: &FlexConfig) -> FlexStreamConfigV3 {
    FlexStreamConfigV3 {
        size: mem::size_of::<FlexStreamConfigV3>().try_into().unwrap(),
        version: 3,
        config: base,
        vad: ptr::null(),
        exclude_pids: ptr::null(),
        exclude_pids_len: 0,
        secondary_output: ptr::null(),
        windows_classic_loopback_keepalive: 0,
    }
}
fn options() -> FlexOptionsV4 {
    FlexOptionsV4 {
        size: mem::size_of::<FlexOptionsV4>().try_into().unwrap(),
        version: 4,
        strict: 0,
        ring_capacity_chunks: ptr::null(),
    }
}
fn v4(base: &FlexStreamConfigV3, options: *const FlexOptionsV4) -> FlexStreamConfigV4 {
    FlexStreamConfigV4 {
        size: mem::size_of::<FlexStreamConfigV4>().try_into().unwrap(),
        version: 4,
        base,
        options,
    }
}
unsafe fn invalid_open(config: *const FlexStreamConfigV4) {
    let mut out = ptr::dangling_mut();
    assert_eq!(
        open_with(config, &mut out, |_, _| panic!(
            "native access before validation"
        )),
        code::FLEX_INVALID_ARG
    );
    assert!(out.is_null());
    assert!(!crate::flexaudio_last_error().is_null());
}

#[test]
fn envelopes_validate_header_before_following_pointers() {
    #[repr(C, align(8))]
    struct Header(u32, u32);
    unsafe {
        invalid_open(ptr::null());
        invalid_open(ptr::dangling::<u8>().cast());
        for header in [Header(8, 4), Header(24, 3), Header(u32::MAX, 4)] {
            invalid_open(ptr::from_ref(&header).cast());
        }
        let base = base();
        let base3 = v3(&base);
        let mut opts = options();
        let mut config = v4(&base3, &opts);
        config.base = ptr::from_ref(&Header(8, 3)).cast();
        invalid_open(&config);
        config.base = &base3;
        config.options = ptr::from_ref(&Header(8, 4)).cast();
        invalid_open(&config);
        config.options = &opts;
        opts.strict = 3;
        config.options = &opts;
        invalid_open(&config);
        opts.strict = 1;
        opts.ring_capacity_chunks = &0;
        config.options = &opts;
        invalid_open(&config);
        opts.ring_capacity_chunks = ptr::dangling::<u8>().cast();
        config.options = &opts;
        invalid_open(&config);
        assert_eq!(
            flexaudio_open_v4(&config, ptr::null_mut()),
            code::FLEX_INVALID_ARG
        );
        assert_eq!(
            flexaudio_open_v4(&config, ptr::dangling_mut::<u8>().cast()),
            code::FLEX_INVALID_ARG
        );
    }
}

#[test]
fn strict_rejection_is_complete_readable_and_precedes_model_and_backend_work() {
    unsafe {
        let mut base = base();
        base.silence_probe = 2;
        base.exclude_self = 1;
        let mut base3 = v3(&base);
        // Valid attachment parameters; strict rejection must precede constructing its model.
        let vad = crate::vad_integration::FlexStreamVadOptions {
            params: crate::vad::flexaudio_vad_default_options(),
            provisional: 0,
            tap: crate::vad_integration::FLEX_VAD_PRIMARY,
        };
        base3.vad = &vad;
        let mut opts = options();
        opts.strict = 1;
        let config = v4(&base3, &opts);
        let mut out = ptr::dangling_mut();
        let calls = AtomicUsize::new(0);
        assert_eq!(
            open_with(&config, &mut out, |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("unexpected native access")
            }),
            code::FLEX_IGNORED_SETTINGS
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(out.is_null());
        assert!(CStr::from_ptr(crate::flexaudio_last_error())
            .to_str()
            .unwrap()
            .contains("ignored"));
        let owner = flexaudio_last_error_info_v2();
        let mut kind = 0;
        assert_eq!(flexaudio_error_kind_v2(owner, &mut kind), 1);
        assert_eq!(kind, -13);
        let mut report = ptr::null();
        assert_eq!(flexaudio_error_settings_v4(owner, &mut report), 1);
        let mut revision = 99;
        let mut count = 0;
        assert_eq!(
            flexaudio_settings_count_v4(report, &mut revision, &mut count),
            0
        );
        assert_eq!((revision, count), (0, 20));
        let mut ignored = Vec::new();
        for index in 0..count {
            let mut row = mem::MaybeUninit::uninit();
            assert_eq!(
                flexaudio_settings_item_v4(report, index, row.as_mut_ptr()),
                0
            );
            let row = row.assume_init();
            if row.status == FLEX_SETTING_IGNORED {
                ignored.push(CStr::from_ptr(row.setting).to_str().unwrap());
                assert_eq!(
                    CStr::from_ptr(row.reason).to_str().unwrap(),
                    "wrongSourceKind"
                );
            } else {
                assert!(row.reason.is_null());
            }
            if CStr::from_ptr(row.setting) == c"vad" {
                assert!(row.requested.value.boolean);
            }
        }
        assert_eq!(ignored, ["excludeSelf", "silenceProbe"]);
        flexaudio_error_info_free_v2(owner);
    }
}

#[test]
fn decoded_defaults_controls_and_fixed_processing_use_shared_resolution() {
    let mut base = base();
    let initial = flexaudio::resolve_settings(
        &SettingsRequest::new(StreamConfig::default()).attachments(
            attachments(&base, 0)
                .denoise(true)
                .vad(true)
                .secondary_encoding("s16"),
        ),
    )
    .unwrap();
    base.denoise = 1;
    let candidate = SettingsRequest::new(StreamConfig::default())
        .attachments(attachments(&base, 0).omitted_on_switch([
            K::Vad,
            K::SecondaryEncoding,
            K::SecondaryOutput,
        ]))
        .previous(&initial);
    let report = flexaudio::resolve_settings(&candidate).unwrap();
    let denoise = report.get(K::Denoise).unwrap();
    assert!(
        matches!(denoise.outcome, flexaudio::SettingOutcome::Ignored(reason) if reason.as_str() == "fixedAtOpen")
    );
    assert_eq!(denoise.effective, flexaudio::SettingValue::Bool(true));
    for key in [K::Gain, K::RingCapacityChunks, K::Vad, K::SecondaryEncoding] {
        assert_eq!(
            report.get(key).unwrap().outcome,
            flexaudio::SettingOutcome::NotRequested
        );
        assert_eq!(
            report.get(key).unwrap().effective,
            initial.get(key).unwrap().effective
        );
    }
    unsafe {
        let mut config = StreamConfig::default();
        let mut current = OpenOptions::default().strict(true);
        controls(ptr::null(), &mut config, &mut current).unwrap();
        assert!(current.strict);
        let mut opts = options();
        opts.strict = 2;
        opts.ring_capacity_chunks = &17;
        controls(&opts, &mut config, &mut current).unwrap();
        assert!(!current.strict);
        assert_eq!(config.ring_capacity_chunks, 17);
        opts.strict = 0;
        opts.ring_capacity_chunks = ptr::null();
        controls(&opts, &mut config, &mut current).unwrap();
        assert_eq!(config.ring_capacity_chunks, 17);
    }
}

#[test]
fn snapshots_own_strings_pids_and_formats_and_validate_borrowed_views() {
    unsafe {
        let mut config = StreamConfig::new(flexaudio::SourceKind::Mix);
        config.mix_mic_device_id = Some("input-selector".into());
        config.exclude_pids = vec![17, 17, 19];
        let report = flexaudio::resolve_settings(&SettingsRequest::new(config)).unwrap();
        let snapshot = Box::into_raw(Box::new(FlexSettingsV4::new(&report)));
        let error = FlexErrorInfoV2::new(flexaudio::Error::IgnoredSettings {
            report: report.clone(),
        });
        drop(report);
        let mut view = ptr::null();
        assert_eq!(flexaudio_error_settings_v4(&error, &mut view), 1);
        for owner in [snapshot.cast_const(), view] {
            let mut rev = 99;
            let mut count = 0;
            assert_eq!(flexaudio_settings_count_v4(owner, &mut rev, &mut count), 0);
            assert_eq!((rev, count), (0, 20));
            for index in 0..count {
                let mut row = mem::MaybeUninit::uninit();
                assert_eq!(
                    flexaudio_settings_item_v4(owner, index, row.as_mut_ptr()),
                    0
                );
                let row = row.assume_init();
                match CStr::from_ptr(row.setting).to_str().unwrap() {
                    "mixMicDeviceId" => {
                        assert_eq!(CStr::from_ptr(row.requested.value.text), c"input-selector")
                    }
                    "excludePids" => {
                        let value = row.effective.value.pids;
                        assert_eq!(std::slice::from_raw_parts(value.data, value.len), [17, 19]);
                        assert_eq!(CStr::from_ptr(row.reason), c"exclusionSetNormalized");
                    }
                    "output" => assert_eq!(
                        (
                            row.effective.value.format.sample_rate,
                            row.effective.value.format.channels
                        ),
                        (48000, 2)
                    ),
                    _ => {}
                }
            }
            assert_eq!(
                flexaudio_settings_item_v4(owner, count, mem::MaybeUninit::uninit().as_mut_ptr()),
                -1
            );
            assert_eq!(
                flexaudio_settings_count_v4(owner, ptr::null_mut(), &mut count),
                -1
            );
        }
        let nonsettings = FlexErrorInfoV2::new(flexaudio::Error::DeviceLost);
        assert_eq!(flexaudio_error_settings_v4(&nonsettings, &mut view), 0);
        assert!(view.is_null());
        assert_eq!(flexaudio_error_settings_v4(ptr::null(), &mut view), -1);
        assert!(view.is_null());
        assert_eq!(
            flexaudio_settings_v4(ptr::null(), &mut ptr::dangling_mut()),
            -1
        );
        flexaudio_settings_free_v4(snapshot);
        flexaudio_settings_free_v4(ptr::null_mut());
    }
}

struct Events {
    events: VecDeque<flexaudio::Event>,
    stops: Arc<AtomicUsize>,
}

#[test]
fn freeing_borrowed_settings_is_a_noop_until_error_owner_is_freed() {
    unsafe {
        let report =
            flexaudio::resolve_settings(&SettingsRequest::new(StreamConfig::default())).unwrap();
        let error = Box::into_raw(Box::new(FlexErrorInfoV2::new(
            flexaudio::Error::IgnoredSettings { report },
        )));
        let mut view = ptr::null();
        assert_eq!(flexaudio_error_settings_v4(error, &mut view), 1);
        assert!(!view.is_null());
        for _ in 0..2 {
            // The free operation reads ownership only; it must not mutate a borrowed view.
            flexaudio_settings_free_v4(view.cast_mut());
            let mut revision = u64::MAX;
            let mut count = 0;
            assert_eq!(
                flexaudio_settings_count_v4(view, &mut revision, &mut count),
                0
            );
            assert_eq!((revision, count), (0, 20));
            let mut item = mem::MaybeUninit::uninit();
            assert_eq!(flexaudio_settings_item_v4(view, 0, item.as_mut_ptr()), 0);
            assert!(!item.assume_init().setting.is_null());
            let mut same_view = ptr::null();
            assert_eq!(flexaudio_error_settings_v4(error, &mut same_view), 1);
            assert_eq!(same_view, view);
        }
        flexaudio_error_info_free_v2(error);
    }
}

impl flexaudio::CaptureBackend for Events {
    fn native_format(&self) -> (u32, u16) {
        (48000, 2)
    }
    fn start(&mut self, _: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        Ok(())
    }
    fn stop(&mut self) {
        if self.stops.fetch_add(1, Ordering::SeqCst) == 0 {
            self.events
                .push_back(flexaudio::Event::ClassicLoopbackKeepaliveStopped {
                    reason: flexaudio::HelperStopReason::StreamStopped,
                });
        }
    }
    fn poll_event(&mut self) -> Option<flexaudio::Event> {
        self.events.pop_front()
    }
}

unsafe fn stopped_event_stream(
    events: VecDeque<flexaudio::Event>,
) -> *mut crate::types::FlexStream {
    let base = base();
    let base3 = v3(&base);
    let config = v4(&base3, ptr::null());
    let mut stream = ptr::null_mut();
    assert_eq!(
        open_with(&config, &mut stream, |config, _options| {
            flexaudio::Stream::open(
                config,
                Box::new(Events {
                    events,
                    stops: Arc::new(AtomicUsize::new(0)),
                }),
            )
        }),
        0
    );
    assert_eq!(crate::flexaudio_start(stream), 0);
    assert_eq!(crate::flexaudio_stop(stream), 0);
    stream
}

#[test]
fn ordinary_only_polling_bounds_the_lifecycle_backlog() {
    unsafe {
        let mut events = VecDeque::from([flexaudio::Event::SilenceProbeStarted]);
        events.extend((0..512).map(|_| flexaudio::Event::ClassicLoopbackKeepaliveStarted));
        events.push_back(flexaudio::Event::StreamStalled);
        let stream = stopped_event_stream(events);
        let mut ordinary = mem::MaybeUninit::uninit();
        assert_eq!(
            crate::flexaudio_poll_event(stream, ordinary.as_mut_ptr()),
            1
        );
        assert_eq!(ordinary.assume_init_ref().kind, 1);
        assert_eq!(
            crate::flexaudio_poll_event(stream, ordinary.as_mut_ptr()),
            0
        );
        let mut event = ptr::null_mut();
        for index in 0..256 {
            assert_eq!(flexaudio_poll_lifecycle_event_v4(stream, &mut event), 1);
            let mut kind = 0;
            assert_eq!(flexaudio_lifecycle_event_kind_v4(event, &mut kind), 0);
            assert_eq!(kind, if index == 255 { 4 } else { 3 });
            flexaudio_lifecycle_event_free_v4(event);
        }
        assert_eq!(flexaudio_poll_lifecycle_event_v4(stream, &mut event), 0);
        assert!(event.is_null());
        crate::flexaudio_free(stream);
    }
}

#[test]
fn event_polls_defensively_serialize_drain_and_both_queues() {
    unsafe {
        let mut events = VecDeque::new();
        for _ in 0..64 {
            events.extend([
                flexaudio::Event::SilenceProbeStarted,
                flexaudio::Event::StreamStalled,
                flexaudio::Event::SilenceProbeStopped {
                    reason: flexaudio::HelperStopReason::Recovery,
                },
            ]);
        }
        let stream = stopped_event_stream(events);
        let shared = std::sync::atomic::AtomicPtr::new(stream);
        let barrier = std::sync::Barrier::new(3);
        // Exercise the defensive locking with no PCM, stop, switch, or free overlapping.
        // AtomicPtr shares the address without creating aliased references to FlexStream.
        std::thread::scope(|scope| {
            let ordinary = scope.spawn(|| {
                barrier.wait();
                let mut count = 0;
                let mut out = mem::MaybeUninit::uninit();
                loop {
                    match crate::flexaudio_poll_event(
                        shared.load(Ordering::Relaxed),
                        out.as_mut_ptr(),
                    ) {
                        1 => {
                            assert_eq!(out.assume_init_ref().kind, 1);
                            count += 1;
                        }
                        0 => return count,
                        status => panic!("ordinary poll failed: {status}"),
                    }
                }
            });
            let versioned = scope.spawn(|| {
                barrier.wait();
                let mut count = 0;
                let mut out = ptr::null_mut();
                loop {
                    match flexaudio_poll_event_v2(shared.load(Ordering::Relaxed), &mut out) {
                        1 => {
                            flexaudio_event_free_v2(out);
                            count += 1;
                        }
                        0 => return count,
                        status => panic!("versioned poll failed: {status}"),
                    }
                }
            });
            let lifecycle = scope.spawn(|| {
                barrier.wait();
                let mut out = ptr::null_mut();
                for index in 0..129 {
                    assert_eq!(
                        flexaudio_poll_lifecycle_event_v4(shared.load(Ordering::Relaxed), &mut out),
                        1
                    );
                    let mut kind = 0;
                    assert_eq!(flexaudio_lifecycle_event_kind_v4(out, &mut kind), 0);
                    assert_eq!(kind, if index == 128 { 4 } else { 1 + index % 2 });
                    flexaudio_lifecycle_event_free_v4(out);
                }
                assert_eq!(
                    flexaudio_poll_lifecycle_event_v4(shared.load(Ordering::Relaxed), &mut out),
                    0
                );
            });
            assert_eq!(ordinary.join().unwrap() + versioned.join().unwrap(), 64);
            lifecycle.join().unwrap();
        });
        crate::flexaudio_free(stream);
    }
}

#[test]
fn lifecycle_is_independent_of_old_polls_and_survives_stream_destruction() {
    unsafe {
        for first_lifecycle in [false, true] {
            let base = base();
            let base3 = v3(&base);
            let config = v4(&base3, ptr::null());
            let mut stream = ptr::null_mut();
            assert_eq!(
                open_with(&config, &mut stream, |config, _options| {
                    flexaudio::Stream::open(
                        config,
                        Box::new(Events {
                            events: VecDeque::from([
                                flexaudio::Event::SilenceProbeStarted,
                                flexaudio::Event::StreamStalled,
                                flexaudio::Event::SilenceProbeStopped {
                                    reason: flexaudio::HelperStopReason::Cancelled,
                                },
                                flexaudio::Event::ClassicLoopbackKeepaliveStarted,
                                flexaudio::Event::StreamRecovered,
                            ]),
                            stops: Arc::new(AtomicUsize::new(0)),
                        }),
                    )
                }),
                0
            );
            assert_eq!(crate::flexaudio_start(stream), 0);
            assert_eq!(crate::flexaudio_stop(stream), 0);
            let mut life = ptr::null_mut();
            let mut ordinary = mem::MaybeUninit::uninit();
            if first_lifecycle {
                assert_eq!(flexaudio_poll_lifecycle_event_v4(stream, &mut life), 1);
            }
            assert_eq!(
                crate::flexaudio_poll_event(stream, ordinary.as_mut_ptr()),
                1
            );
            assert_eq!(ordinary.assume_init().kind, 1);
            let mut event = ptr::null_mut();
            assert_eq!(flexaudio_poll_event_v2(stream, &mut event), 1);
            let mut kind = -1;
            assert_eq!(flexaudio_event_kind_v2(event, &mut kind), 1);
            assert_eq!(kind, 2);
            flexaudio_event_free_v2(event);
            assert_eq!(flexaudio_poll_event_v2(stream, &mut event), 0);
            if !first_lifecycle {
                assert_eq!(flexaudio_poll_lifecycle_event_v4(stream, &mut life), 1);
            }
            let mut kind = 99;
            assert_eq!(flexaudio_lifecycle_event_kind_v4(life, &mut kind), 0);
            assert_eq!(kind, 1);
            let mut reason = 99;
            assert_eq!(flexaudio_lifecycle_event_reason_v4(life, &mut reason), 0);
            assert_eq!(reason, 99);
            let mut message = c"unchanged".as_ptr();
            assert_eq!(flexaudio_lifecycle_event_message_v4(life, &mut message), 0);
            assert_eq!(CStr::from_ptr(message), c"unchanged");
            flexaudio_lifecycle_event_free_v4(life);
            let mut held = Vec::new();
            while flexaudio_poll_lifecycle_event_v4(stream, &mut life) == 1 {
                held.push(life);
            }
            assert!(life.is_null());
            crate::flexaudio_free(stream);
            assert_eq!(held.len(), 3);
            for (event, (expected_kind, expected_reason)) in
                held.into_iter()
                    .zip([(2, Some(10)), (3, None), (4, Some(5))])
            {
                assert_eq!(flexaudio_lifecycle_event_kind_v4(event, &mut kind), 0);
                assert_eq!(kind, expected_kind);
                assert_eq!(
                    flexaudio_lifecycle_event_reason_v4(event, &mut reason),
                    i32::from(expected_reason.is_some())
                );
                if let Some(expected) = expected_reason {
                    assert_eq!(reason, expected);
                }
                flexaudio_lifecycle_event_free_v4(event);
            }
        }
        let mut event = ptr::dangling_mut();
        assert_eq!(
            flexaudio_poll_lifecycle_event_v4(ptr::null_mut(), &mut event),
            -1
        );
        assert!(event.is_null());
        assert_eq!(flexaudio_lifecycle_event_kind_v4(ptr::null(), &mut 0), -1);
        assert_eq!(flexaudio_lifecycle_event_reason_v4(ptr::null(), &mut 0), -1);
        assert_eq!(
            flexaudio_lifecycle_event_message_v4(ptr::null(), &mut ptr::null()),
            -1
        );
        flexaudio_lifecycle_event_free_v4(ptr::null_mut());
    }
}

#[test]
fn arrival_alias_preserves_every_i64_bit_and_validates_pointers() {
    unsafe {
        let mut chunk: crate::vad_integration::FlexChunkV2 = mem::zeroed();
        for expected in [i64::MIN, -1, 0, 1, i64::MAX] {
            chunk.chunk.pts_ns = expected;
            let mut actual = 0;
            assert_eq!(
                crate::discovery_v4::flexaudio_chunk_arrival_ns_v4(&chunk.chunk, &mut actual),
                0
            );
            assert_eq!(actual, expected);
        }
        assert_eq!(
            crate::discovery_v4::flexaudio_chunk_arrival_ns_v4(ptr::null(), &mut 0),
            -1
        );
        assert_eq!(
            crate::discovery_v4::flexaudio_chunk_arrival_ns_v4(&chunk.chunk, ptr::null_mut()),
            -1
        );
        assert_eq!(
            crate::discovery_v4::flexaudio_secondary_arrival_ns_v4(ptr::null(), &mut 0),
            -1
        );
    }
}

#[test]
fn switch_validates_envelopes_and_strict_failures_preserve_the_old_source() {
    unsafe {
        let base = base();
        let base3 = v3(&base);
        let config = v4(&base3, ptr::null());
        let stops = Arc::new(AtomicUsize::new(0));
        let mut stream = ptr::null_mut();
        assert_eq!(
            open_with(&config, &mut stream, |config, _| flexaudio::Stream::open(
                config,
                Box::new(Events {
                    events: VecDeque::new(),
                    stops: stops.clone()
                })
            )),
            0
        );
        assert_eq!(crate::flexaudio_start(stream), 0);
        let mut replacement: FlexConfig = mem::zeroed();
        replacement.denoise = 1;
        let mut base_switch = FlexSwitchConfigV3 {
            size: mem::size_of::<FlexSwitchConfigV3>().try_into().unwrap(),
            version: 3,
            config: &replacement,
            exclude_pids: ptr::null(),
            exclude_pids_len: 0,
            windows_classic_loopback_keepalive: 0,
        };
        let mut opts = options();
        opts.strict = 1;
        let mut switch = FlexSwitchConfigV4 {
            size: mem::size_of::<FlexSwitchConfigV4>().try_into().unwrap(),
            version: 4,
            base: &base_switch,
            options: &opts,
        };
        assert_eq!(flexaudio_switch_source_v4(stream, &switch), -13);
        let candidate = error::last_audio_error().unwrap();
        let report = candidate.ignored_settings().unwrap();
        assert_eq!(report.revision, 1);
        assert_eq!(
            report.get(K::Denoise).unwrap().requested,
            flexaudio::SettingValue::Bool(true)
        );
        assert_eq!(
            report.get(K::Denoise).unwrap().effective,
            flexaudio::SettingValue::Bool(false)
        );
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        assert!(!(*stream).inner.open_options().strict);
        assert!((*stream).inner.effective_settings().is_none());
        for bad_version in [0, 3, 5, u32::MAX] {
            switch.version = bad_version;
            assert_eq!(flexaudio_switch_source_v4(stream, &switch), -1);
        }
        switch.version = 4;
        base_switch.version = 2;
        switch.base = &base_switch;
        assert_eq!(flexaudio_switch_source_v4(stream, &switch), -1);
        assert_eq!(flexaudio_switch_source_v4(ptr::null_mut(), &switch), -1);
        assert_eq!(flexaudio_switch_source_v4(stream, ptr::null()), -1);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        crate::flexaudio_free(stream);
    }
}

#[test]
fn namespaced_device_projection_preserves_ids_and_owns_only_device_strings() {
    use crate::discovery_v4::*;
    unsafe {
        let mut entries = Vec::new();
        for (namespace, tag) in [
            (
                flexaudio::DeviceIdNamespace::CpalDeviceName,
                c"cpalDeviceName",
            ),
            (
                flexaudio::DeviceIdNamespace::PipeWireNodeName,
                c"pipeWireNodeName",
            ),
            (
                flexaudio::DeviceIdNamespace::WasapiOutputSelector,
                c"wasapiOutputSelector",
            ),
            (
                flexaudio::DeviceIdNamespace::CoreAudioOutputSelector,
                c"coreAudioOutputSelector",
            ),
            (flexaudio::DeviceIdNamespace::Unknown, c"unknown"),
        ] {
            // Deliberately identical literals: provenance comes only from the provider.
            let mut info = flexaudio::DeviceInfo::new(
                "selector".into(),
                "duplicate name".into(),
                flexaudio::SourceKind::Mic,
            );
            info.sample_rate = 48000;
            info.channels = 2;
            info.format_available = true;
            info.is_loopback = false;
            info.is_default = false;
            let old = crate::convert::device_info_to_c(info.clone());
            let entry = device(flexaudio::NamespacedDevice::new(info, namespace));
            assert_eq!(CStr::from_ptr(entry.device.id), CStr::from_ptr(old.id));
            assert_eq!(CStr::from_ptr(entry.device.name), CStr::from_ptr(old.name));
            assert_eq!(CStr::from_ptr(entry.id_namespace), tag);
            drop(std::ffi::CString::from_raw(old.id));
            drop(std::ffi::CString::from_raw(old.name));
            entries.push(entry);
        }
        let len = entries.len();
        let entries = Box::into_raw(entries.into_boxed_slice()).cast::<FlexNamespacedDeviceV4>();
        let namespace = (*entries).id_namespace;
        flexaudio_namespaced_devices_free_v4(entries, len);
        assert_eq!(CStr::from_ptr(namespace), c"cpalDeviceName");
        flexaudio_namespaced_devices_free_v4(ptr::null_mut(), 0);
        let mut out = ptr::dangling_mut();
        assert_eq!(
            flexaudio_devices_with_namespaces_v4(&mut out, ptr::null_mut()),
            -1
        );
        assert!(out.is_null());
        assert_eq!(
            flexaudio_devices_with_namespaces_v4(ptr::null_mut(), &mut 0),
            -1
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn successful_facade_open_has_full_settings_and_snapshot_survives_stream_free() {
    // Constructing a process backend does not connect to the server until start.
    unsafe {
        let mut base = base();
        base.kind = crate::FLEX_SOURCE_KIND_PROCESS;
        base.process_id = 17;
        base.denoise = 1;
        let secondary = FlexSecondaryOutputV3 {
            size: mem::size_of::<FlexSecondaryOutputV3>().try_into().unwrap(),
            version: 3,
            rate: 16000,
            channels: 1,
            encoding: FLEX_SECONDARY_S16,
        };
        let mut base3 = v3(&base);
        base3.secondary_output = &secondary;
        let mut opts = options();
        opts.ring_capacity_chunks = &7;
        let config = v4(&base3, &opts);
        let mut stream = ptr::null_mut();
        assert_eq!(flexaudio_open_v4(&config, &mut stream), 0);
        let mut snapshot = ptr::null_mut();
        assert_eq!(flexaudio_settings_v4(stream, &mut snapshot), 0);
        assert_eq!((*stream).inner.config().ring_capacity_chunks, 7);
        crate::flexaudio_free(stream);
        let mut revision = 99;
        let mut count = 0;
        assert_eq!(
            flexaudio_settings_count_v4(snapshot, &mut revision, &mut count),
            0
        );
        assert_eq!((revision, count), (0, 20));
        for index in 0..count {
            let mut row = mem::MaybeUninit::uninit();
            assert_eq!(
                flexaudio_settings_item_v4(snapshot, index, row.as_mut_ptr()),
                0
            );
            let row = row.assume_init();
            match CStr::from_ptr(row.setting).to_str().unwrap() {
                "denoise" => {
                    assert_eq!(row.status, FLEX_SETTING_APPLIED);
                    assert!(row.effective.value.boolean);
                }
                "secondaryEncoding" => {
                    assert_eq!(row.status, FLEX_SETTING_APPLIED);
                    assert_eq!(CStr::from_ptr(row.effective.value.text), c"s16");
                }
                "ringCapacityChunks" => assert_eq!(row.effective.value.unsigned_value, 7),
                _ => {}
            }
        }
        flexaudio_settings_free_v4(snapshot);
    }
}

#[test]
fn c_requests_cover_static_applicability_before_native_access() {
    let mut cases = Vec::new();
    for kind in [2, 3] {
        cases.push((kind, "deviceId", "wrongSourceKind", false));
    }
    for kind in [0, 1, 3] {
        cases.push((kind, "targetPid", "wrongSourceKind", false));
        cases.push((kind, "mode", "wrongSourceKind", false));
    }
    for kind in [0, 2] {
        cases.push((kind, "excludeSelf", "wrongSourceKind", false));
        cases.push((kind, "excludePids", "wrongSourceKind", false));
        cases.push((
            kind,
            "windowsClassicLoopbackKeepalive",
            "wrongSourceKind",
            false,
        ));
    }
    for kind in [0, 1, 2] {
        for setting in [
            "mixMicDeviceId",
            "mixSystemDeviceId",
            "mixMicGain",
            "mixSystemGain",
        ] {
            cases.push((kind, setting, "wrongSourceKind", false));
        }
    }
    cases.push((0, "silenceProbe", "wrongSourceKind", false));
    if !cfg!(target_os = "macos") {
        for kind in [1, 2, 3] {
            cases.push((kind, "silenceProbe", "unsupportedPlatform", false));
        }
    }
    for kind in [1, 3] {
        if !cfg!(target_os = "windows") {
            cases.push((
                kind,
                "windowsClassicLoopbackKeepalive",
                "unsupportedPlatform",
                false,
            ));
        } else {
            cases.push((
                kind,
                "windowsClassicLoopbackKeepalive",
                "processLoopbackPath",
                true,
            ));
        }
    }
    if cfg!(any(target_os = "linux", target_os = "windows")) {
        cases.push((1, "deviceId", "exclusionDeviceUnscoped", true));
        cases.push((3, "mixSystemDeviceId", "exclusionDeviceUnscoped", true));
    }
    for (kind, setting, expected, exclusions) in cases {
        unsafe {
            let mut base = base();
            base.kind = kind;
            if kind == 2 {
                base.process_id = 17;
            }
            base.exclude_self = u8::from(exclusions);
            let pids = [19];
            let mut base3 = v3(&base);
            match setting {
                "deviceId" => base.device_id = c"selector".as_ptr(),
                "targetPid" => base.process_id = 19,
                "mode" => base.mode = 1,
                "excludeSelf" => base.exclude_self = 1,
                "excludePids" => {
                    base3.exclude_pids = pids.as_ptr();
                    base3.exclude_pids_len = 1;
                }
                "mixMicDeviceId" => base.mix_mic_device_id = c"input".as_ptr(),
                "mixSystemDeviceId" => base.mix_system_device_id = c"output".as_ptr(),
                "mixMicGain" => base.mix_mic_gain = 2.0,
                "mixSystemGain" => base.mix_system_gain = 2.0,
                "silenceProbe" => base.silence_probe = 2,
                "windowsClassicLoopbackKeepalive" => base3.windows_classic_loopback_keepalive = 2,
                _ => unreachable!(),
            }
            base3.config = &base;
            let mut opts = options();
            opts.strict = 1;
            let config = v4(&base3, &opts);
            let mut out = ptr::null_mut();
            assert_eq!(
                open_with(&config, &mut out, |_, _| panic!(
                    "native access before static rejection"
                )),
                -13,
                "kind={kind} setting={setting}"
            );
            let error = error::last_audio_error().unwrap();
            let entry = error
                .ignored_settings()
                .unwrap()
                .entries
                .iter()
                .find(|entry| entry.setting.as_str() == setting)
                .unwrap();
            assert!(
                matches!(entry.outcome, flexaudio::SettingOutcome::Ignored(reason) if reason.as_str() == expected),
                "kind={kind} setting={setting}: {:?}",
                entry.outcome
            );
        }
    }
}
