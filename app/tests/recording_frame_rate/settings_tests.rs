use super::*;
use serde_json::json;

#[test]
fn recording_frame_rate_defaults_to_source_without_writing() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let absent = directory.path().join("absent").join(SETTINGS_FILE);

    assert_eq!(
        recording_frame_rate_from(&absent).expect("missing settings should load"),
        RecordingFrameRate::Source
    );
    assert!(
        !absent
            .parent()
            .expect("settings path has a parent")
            .exists(),
        "reading a missing preference must not create the settings directory"
    );

    let existing = directory.path().join(SETTINGS_FILE);
    let original = br#"{"record_fps":48,"other":{"keep":[null,true,"unchanged"]}}"#;
    std::fs::write(&existing, original).expect("preferences fixture should be written");
    assert_eq!(
        recording_frame_rate_from(&existing).expect("missing key should load"),
        RecordingFrameRate::Source
    );
    assert_eq!(
        std::fs::read(&existing).expect("preferences should remain readable"),
        original,
        "reading the default must not rewrite existing settings"
    );
}

#[test]
fn recording_frame_rate_round_trips_every_choice_and_preserves_other_settings() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let path = directory.path().join(SETTINGS_FILE);
    let original = json!({
        "record_fps": 48,
        "theme": "Dark",
        "nested": {"keep": [null, true, "é", {"value": -7}]},
        "capture_cursor": false
    });
    std::fs::write(
        &path,
        serde_json::to_vec(&original).expect("fixture should serialize"),
    )
    .expect("preferences fixture should be written");

    for (rate, stored) in [
        (RecordingFrameRate::Source, "source"),
        (RecordingFrameRate::Fps15, "15"),
        (RecordingFrameRate::Fps24, "24"),
        (RecordingFrameRate::Fps30, "30"),
        (RecordingFrameRate::Fps60, "60"),
    ] {
        set_recording_frame_rate_in(&path, rate).expect("valid frame rate should save");
        assert_eq!(
            recording_frame_rate_from(&path).expect("saved frame rate should load"),
            rate,
            "stored value {stored:?} should reload"
        );
        let actual: Value = serde_json::from_slice(
            &std::fs::read(&path).expect("saved preferences should be readable"),
        )
        .expect("saved preferences should be valid JSON");
        let mut expected = original.clone();
        expected[RECORDING_FRAME_RATE_KEY] = json!(stored);
        assert_eq!(
            actual, expected,
            "saving {stored:?} must preserve all other data"
        );
    }
}

#[test]
fn recording_frame_rate_rejects_invalid_values_without_rewriting() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let path = directory.path().join(SETTINGS_FILE);
    let invalid = [
        json!(null),
        json!(true),
        json!(false),
        json!(30),
        json!(30.5),
        json!([]),
        json!({}),
        json!(""),
        json!("SOURCE"),
        json!(" 30"),
        json!("30 "),
        json!("30fps"),
        json!("0"),
        json!("-1"),
        json!("120"),
        json!("６０"),
    ];
    for value in invalid {
        let fixture = json!({
            "linux_recording_frame_rate": value,
            "record_fps": 29,
            "other": {"keep": [1, null, "original"]}
        });
        let bytes = serde_json::to_vec(&fixture).expect("fixture should serialize");
        std::fs::write(&path, &bytes).expect("invalid preference should be written");
        let error = recording_frame_rate_from(&path)
            .expect_err("invalid recording frame rate should be rejected");
        let expected = if fixture[RECORDING_FRAME_RATE_KEY].is_string() {
            "unknown recording frame rate preference"
        } else {
            "recording frame rate preference must be text"
        };
        assert!(error.contains(expected), "fixture {fixture}: {error}");
        assert_eq!(
            std::fs::read(&path).expect("invalid preferences should remain readable"),
            bytes,
            "rejecting {fixture} must not rewrite unrelated data"
        );
    }
}

#[test]
fn recording_frame_rate_can_repair_an_invalid_value_without_losing_other_data() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let path = directory.path().join(SETTINGS_FILE);
    let original = json!({
        "linux_recording_frame_rate": {"invalid": true},
        "record_fps": 75,
        "other": {"keep": ["a", null, 3]}
    });
    std::fs::write(
        &path,
        serde_json::to_vec(&original).expect("fixture should serialize"),
    )
    .expect("invalid preference should be written");
    assert!(recording_frame_rate_from(&path).is_err());
    set_recording_frame_rate_in(&path, RecordingFrameRate::Fps24)
        .expect("an explicit valid choice should repair the preference");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).expect("settings should load"))
        .expect("saved settings should be JSON");
    let mut expected = original;
    expected[RECORDING_FRAME_RATE_KEY] = json!("24");
    assert_eq!(saved, expected);
}

#[test]
fn recording_frame_rate_rejects_malformed_settings_without_corruption() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let path = directory.path().join(SETTINGS_FILE);
    for bytes in [
        b"{".as_slice(),
        br#"{"linux_recording_frame_rate":"30","other":"#,
        b"[]",
        b"null",
        b"42",
        b"\"text\"",
        b"\xff",
    ] {
        std::fs::write(&path, bytes).expect("malformed fixture should be written");
        let read_error =
            recording_frame_rate_from(&path).expect_err("malformed settings must not load");
        let write_error = set_recording_frame_rate_in(&path, RecordingFrameRate::Fps15)
            .expect_err("malformed settings must not be overwritten");
        for error in [read_error, write_error] {
            assert!(
                error.contains("could not parse settings") || error.contains("JSON object"),
                "fixture {bytes:?}: {error}"
            );
        }
        assert_eq!(
            std::fs::read(&path).expect("malformed fixture should remain readable"),
            bytes,
            "failed reads and writes must preserve every original byte"
        );
    }
}

#[test]
fn recording_frame_rate_selector_maps_choices_and_rejects_out_of_range_indices() {
    for (index, rate, label, stored) in [
        (0, RecordingFrameRate::Source, "Source", "source"),
        (1, RecordingFrameRate::Fps15, "15 fps", "15"),
        (2, RecordingFrameRate::Fps24, "24 fps", "24"),
        (3, RecordingFrameRate::Fps30, "30 fps", "30"),
        (4, RecordingFrameRate::Fps60, "60 fps", "60"),
    ] {
        assert_eq!(RecordingFrameRate::from_index(index), Ok(rate));
        assert_eq!(rate.index(), index);
        assert_eq!(rate.label(), label);
        assert_eq!(rate.stored_name(), stored);
        assert_eq!(RecordingFrameRate::from_stored_name(stored), Ok(rate));
    }
    for index in [i32::MIN, -1, 5, i32::MAX] {
        assert_eq!(
            RecordingFrameRate::from_index(index),
            Err("select a recording frame rate".to_owned()),
            "invalid selector index {index} must be rejected"
        );
    }
}

#[test]
fn recording_frame_rate_save_respects_the_shared_settings_lock() {
    let directory = tempfile::tempdir().expect("temporary directory should be available");
    let path = directory.path().join(SETTINGS_FILE);
    let original = br#"{"linux_recording_frame_rate":"15","record_fps":144,"other":{"keep":true}}"#;
    std::fs::write(&path, original).expect("preferences fixture should be written");
    let lock = lock_settings(&path).expect("competing settings lock should be acquired");
    let error = set_recording_frame_rate_in(&path, RecordingFrameRate::Fps60)
        .expect_err("saving under a competing lock must fail");
    assert!(error.contains("could not lock"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("settings should remain readable"),
        original
    );
    drop(lock);

    set_recording_frame_rate_in(&path, RecordingFrameRate::Fps60)
        .expect("saving after the lock is released should succeed");
    let actual: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("settings should load"))
            .expect("saved settings should be JSON");
    assert_eq!(
        actual,
        json!({"linux_recording_frame_rate":"60","record_fps":144,"other":{"keep":true}})
    );
}
