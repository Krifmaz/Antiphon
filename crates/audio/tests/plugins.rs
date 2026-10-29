//! The plugin host against a real plugin library: the test gain plugin in
//! `crates/test-plugins`, in each of its three formats, loaded the way a
//! user's plugin is and run on a block of audio.

#![cfg(desktop)]

use std::path::{Path, PathBuf};

use antiphon_audio::plugins::{load, scan_in, ParamValue, PluginConfig, PluginFormat};

/// The built test plugin library, which Cargo puts beside this test.
fn library() -> PathBuf {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let ext = if cfg!(windows) { "dll" } else if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let prefix = if cfg!(windows) { "antiphon_test_plugins" } else { "libantiphon_test_plugins" };
    std::fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with(prefix) && p.extension().is_some_and(|e| e == ext)
        })
        .unwrap_or_else(|| panic!("the test plugin library isn't in {}", deps.display()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("antiphon-plugins-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `config` on a ramp and returns what came out.
fn run(config: &PluginConfig) -> Vec<f32> {
    let mut stage = load(config, 48_000).unwrap_or_else(|e| panic!("{e}"));
    // Two blocks of different sizes, the second larger than the first, so
    // buffers have to grow and be connected again.
    let mut a: Vec<f32> = (0..64).map(|i| i as f32 / 100.0).collect();
    stage.process(&mut a);
    let mut b: Vec<f32> = (0..5000).map(|i| (i % 100) as f32 / 100.0).collect();
    stage.process(&mut b);
    a.extend(b);
    a
}

fn expect_gain(out: &[f32], gain: f32) {
    let input: Vec<f32> = (0..64).map(|i| i as f32 / 100.0).chain((0..5000).map(|i| (i % 100) as f32 / 100.0)).collect();
    for (i, (o, x)) in out.iter().zip(&input).enumerate() {
        assert!((o - x * gain).abs() < 1e-5, "sample {i}: {o} != {x} x {gain}");
    }
}

fn find(dir: &Path, format: PluginFormat) -> PluginConfig {
    let found = scan_in(format, dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "Antiphon Test Gain");
    found[0].to_config()
}

#[test]
fn a_ladspa_plugin_is_found_and_runs_with_its_saved_gain() {
    let dir = scratch("ladspa");
    let lib = library();
    std::fs::copy(&lib, dir.join(lib.file_name().unwrap())).unwrap();
    let mut config = find(&dir, PluginFormat::Ladspa);
    assert_eq!(config.params, vec![ParamValue { id: "0".into(), value: 1.0 }], "the default from its hints");
    expect_gain(&run(&config), 1.0);
    config.params[0].value = 2.5;
    expect_gain(&run(&config), 2.5);
    config.params[0].value = 99.0;
    expect_gain(&run(&config), 4.0);
}

#[test]
fn an_lv2_plugin_is_found_and_runs_with_its_saved_gain() {
    let dir = scratch("lv2");
    antiphon_test_plugins::write_lv2_bundle(&dir, &library()).unwrap();
    let mut config = find(&dir, PluginFormat::Lv2);
    assert_eq!(config.id, antiphon_test_plugins::LV2_URI);
    assert_eq!(config.params, vec![ParamValue { id: "gain".into(), value: 1.0 }]);
    expect_gain(&run(&config), 1.0);
    config.params[0].value = 0.5;
    expect_gain(&run(&config), 0.5);
}

#[test]
fn a_vst3_plugin_is_found_and_runs_with_its_saved_gain() {
    let dir = scratch("vst3");
    antiphon_test_plugins::vst::write_bundle(&dir, &library()).unwrap();
    let mut config = find(&dir, PluginFormat::Vst3);
    assert_eq!(config.id, antiphon_test_plugins::vst::CID_TEXT);
    // The bypass switch is the chain's job, so only the gain is offered.
    assert_eq!(config.params, vec![ParamValue { id: "0".into(), value: 0.5 }]);
    expect_gain(&run(&config), 1.0);
    config.params[0].value = 0.25;
    expect_gain(&run(&config), 0.5);
}
