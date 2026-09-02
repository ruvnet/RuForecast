//! Build real (never synthetic) train/test JsonlWindow shards from a real
//! household vitals JSONL export. LOCAL-DEV-ONLY: input/output paths point
//! at untracked /tmp scratch files, never a repo path. Real, identifiable
//! biometric data (PrivacyClass::P4) — informal consent noted, temporal
//! (not entity) holdout, single physical sensor.
//!
//! Input line shape: {"poll_index":N,"unix_ts":N,"sample":{"vital_signs":{
//!   "heart_rate_bpm":f,"breathing_rate_bpm":f,"signal_quality":f, ...}}}
//!
//! Usage: cargo run --example real_data_windows -- <input.jsonl> <out_dir> <embargo_s>

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use ruforecast_core::{
    CanonicalDigest, DataPolicy, HoldoutKey, NormalizationPolicy, PrivacyClass, QuantileSet,
    SeriesKey, SplitMember, SplitStrategy, TemporalSplitPlan, TimeRange,
};
use ruforecast_model::ForecastModelConfig;
use ruforecast_train::config::{
    DatasetInput, DatasetSource, JobId, LocalTrainSpecWire, LocalTrainingRequestWire, ModelProfile,
    OptimizerSpec, RelativeDataPath, Sha256Digest, TrainingBudget, TrainingDevice,
};
use ruforecast_train::corpus::JsonlWindow;

const VARIATES: usize = 3;
const STEP_MS: u64 = 1_000;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input = &args[1];
    let out_dir = std::path::PathBuf::from(&args[2]);
    let embargo_s: u64 = args[3].parse()?;
    let train_fraction: f64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(0.70);
    std::fs::create_dir_all(&out_dir)?;

    let file = std::fs::File::open(input)?;
    let reader = BufReader::new(file);
    let mut by_ts: BTreeMap<u64, [f32; VARIATES]> = BTreeMap::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line)?;
        let ts = v["unix_ts"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing unix_ts"))?;
        let vs = &v["sample"]["vital_signs"];
        let hr = vs["heart_rate_bpm"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing hr"))? as f32;
        let br = vs["breathing_rate_bpm"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing br"))? as f32;
        let sq = vs["signal_quality"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing sq"))? as f32;
        by_ts.insert(ts, [hr, br, sq]);
    }
    let min_ts = *by_ts
        .keys()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty input"))?;
    let max_ts = *by_ts.keys().next_back().unwrap();
    let span = (max_ts - min_ts + 1) as usize;
    println!(
        "real samples: {} span_s: {} coverage: {:.4}",
        by_ts.len(),
        span,
        by_ts.len() as f64 / span as f64
    );

    // Uniform 1s grid, real observed_mask where a real sample exists at that
    // second, zero-filled + mask=0 elsewhere (honest gap handling, not
    // fabricated interpolation).
    let mut grid: Vec<[f32; VARIATES]> = vec![[0.0; VARIATES]; span];
    let mut mask: Vec<bool> = vec![false; span];
    for (ts, vals) in &by_ts {
        let idx = (ts - min_ts) as usize;
        grid[idx] = *vals;
        mask[idx] = true;
    }

    let model = ForecastModelConfig::tiny_ci();
    let context_len = model.context_len;
    let horizon = model.horizon;
    let window_len = context_len + horizon;

    // Temporal split: first 70% -> train region, embargo gap, remainder -> test region.
    let train_end = (span as f64 * train_fraction) as usize;
    let test_start = train_end + embargo_s as usize;
    if test_start + window_len > span {
        anyhow::bail!("test region too small after embargo; reduce embargo or extend collection");
    }

    let build_windows = |region_start: usize,
                         region_end: usize,
                         room: &str,
                         device: &str,
                         idx_offset: u64|
     -> Vec<(SeriesKey, JsonlWindow)> {
        let mut out = Vec::new();
        let mut pos = region_start;
        let mut i = 0u64;
        while pos + window_len <= region_end {
            let key = SeriesKey::new(room, device, format!("s{}", idx_offset + i)).unwrap();
            let mut values = Vec::with_capacity(context_len * VARIATES);
            let mut observed_mask = Vec::with_capacity(context_len * VARIATES);
            for row in 0..context_len {
                let g = grid[pos + row];
                let m = mask[pos + row];
                for v in 0..VARIATES {
                    values.push(g[v]);
                    observed_mask.push(u8::from(m));
                }
            }
            let mut targets = vec![0.0f32; VARIATES * horizon];
            let mut target_mask = vec![0u8; VARIATES * horizon];
            for step in 0..horizon {
                let g = grid[pos + context_len + step];
                let m = mask[pos + context_len + step];
                for v in 0..VARIATES {
                    targets[v * horizon + step] = g[v];
                    target_mask[v * horizon + step] = u8::from(m);
                }
            }
            let window = JsonlWindow {
                version: 1,
                series_key: key.clone(),
                context_start_ms: (min_ts + pos as u64) * STEP_MS,
                variates: VARIATES as u16,
                values,
                observed_mask,
                targets,
                target_mask,
            };
            out.push((key, window));
            pos += window_len;
            i += 1;
        }
        out
    };

    let train_windows = build_windows(0, train_end, "real-home-lab-train", "esp32-cohen-train", 0);
    let test_windows = build_windows(
        test_start,
        span,
        "real-home-lab-test",
        "esp32-cohen-test",
        10_000,
    );
    println!(
        "train windows: {} test windows: {} embargo_s: {}",
        train_windows.len(),
        test_windows.len(),
        embargo_s
    );
    if train_windows.is_empty() || test_windows.is_empty() {
        anyhow::bail!("empty train or test window set");
    }

    let train_lines: Vec<String> = train_windows
        .iter()
        .map(|(_, w)| serde_json::to_string(w).unwrap())
        .collect();
    let mut shard = train_lines.join("\n").into_bytes();
    shard.push(b'\n');
    std::fs::write(out_dir.join("train.jsonl"), &shard)?;
    let sha256 = Sha256Digest::of_bytes(&shard);

    let test_lines: Vec<String> = test_windows
        .iter()
        .map(|(_, w)| serde_json::to_string(w).unwrap())
        .collect();
    let mut test_shard = test_lines.join("\n").into_bytes();
    test_shard.push(b'\n');
    std::fs::write(out_dir.join("test.jsonl"), &test_shard)?;

    let train_members: Vec<SplitMember> = train_windows
        .iter()
        .map(|(k, _)| SplitMember::new(k.clone(), TimeRange::new(1, u64::MAX / 2).unwrap()))
        .collect();
    let test_members: Vec<SplitMember> = test_windows
        .iter()
        .map(|(k, _)| SplitMember::new(k.clone(), TimeRange::new(1, u64::MAX / 2).unwrap()))
        .collect();
    let split_plan = TemporalSplitPlan::new(
        SplitStrategy::EntityHoldout(HoldoutKey::Strict),
        train_members,
        vec![],
        test_members,
        horizon,
        STEP_MS,
        embargo_s * 1000,
    )?;

    let feature_schema_digest = CanonicalDigest::of_bytes(
        b"ruview-real-vitals-feature-schema-v1",
        b"heart_rate_bpm,breathing_rate_bpm,signal_quality",
    );
    let policy = DataPolicy::new(
        PrivacyClass::P4,
        "real-home-lab",
        "real-home-lab",
        "real-home-lab",
        "real-data-accuracy-comparison-informal",
        CanonicalDigest::of_bytes(
            b"ruview-real-vitals-policy-v1",
            b"session-scoped-informal-authorization",
        ),
        Some(CanonicalDigest::of_bytes(
            b"ruview-real-vitals-consent-v1",
            b"informal-verbal-authorization-by-device-owner-not-a-formal-consent-record",
        )),
        None,
        None,
        chrono_now_ms().saturating_add(24 * 60 * 60 * 1_000),
        false,
    )?;

    let steps_per_epoch = (train_windows.len() as u64).div_ceil(8);
    let epochs = 60u16;
    let max_optimizer_steps = steps_per_epoch.saturating_mul(u64::from(epochs)).max(1);
    let request = LocalTrainingRequestWire {
        job_id: JobId::new("real-home-lab")?,
        train: LocalTrainSpecWire {
            context_length: context_len,
            horizon,
            step_ms: STEP_MS,
            quantiles: QuantileSet::new(model.quantiles.to_vec())?,
            split_plan,
            normalization: NormalizationPolicy::None,
            dataset_digest: CanonicalDigest::of_bytes(
                b"ruview-jsonl-window-shard-v1",
                sha256.as_bytes(),
            ),
            policy,
        },
        dataset: DatasetSource::Manifest(DatasetInput {
            path: RelativeDataPath::new("train.jsonl")?,
            size_bytes: u64::try_from(shard.len())?,
            sha256,
            window_count: u32::try_from(train_windows.len())?,
            variates: VARIATES as u16,
            feature_schema_digest,
        }),
        model: ModelProfile::TinyCi,
        device: TrainingDevice::Cpu,
        optimizer: OptimizerSpec {
            epochs,
            batch_size: 8,
            learning_rate: 0.001,
            weight_decay: 0.0001,
            gradient_clip_norm: 1.0,
            checkpoint_every_epochs: epochs,
            seed: 11,
        },
        budget: TrainingBudget {
            max_optimizer_steps,
            max_wall_time_seconds: 900,
            max_memory_bytes: 4 * 1024 * 1024 * 1024,
            max_artifact_bytes: 512 * 1024 * 1024,
            max_checkpoints: 1,
        },
    };
    let request_bytes = toml::to_string_pretty(&request)?.into_bytes();
    std::fs::write(out_dir.join("train-local.toml"), &request_bytes)?;
    println!(
        "wrote real train-local.toml, train.jsonl, test.jsonl to {}",
        out_dir.display()
    );
    Ok(())
}

fn chrono_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
