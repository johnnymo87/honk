use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const RECENT_FILENAME: &str = "model-recent.json";
pub const MAX_RECENT_MODELS: usize = 10;

pub fn honk_state_dir() -> anyhow::Result<PathBuf> {
    crate::client::honk_state_dir()
}

pub fn load() -> Vec<String> {
    match honk_state_dir() {
        Ok(dir) => load_from(&dir),
        Err(_) => Vec::new(),
    }
}

pub fn load_from(dir: &Path) -> Vec<String> {
    let path = dir.join(RECENT_FILENAME);
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(ids) = serde_json::from_str::<Vec<String>>(&content) else {
        return Vec::new();
    };
    ids
}

pub fn record(id: &str) -> anyhow::Result<()> {
    let dir = honk_state_dir()?;
    record_in(&dir, id)
}

pub fn record_in(dir: &Path, id: &str) -> anyhow::Result<()> {
    let id = id.trim();
    if id.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let mut models = load_from(dir);
    // Dedup: remove existing instance
    models.retain(|m| m != id);
    // Prepend (MRU)
    models.insert(0, id.to_string());
    // Cap at MAX_RECENT_MODELS
    if models.len() > MAX_RECENT_MODELS {
        models.truncate(MAX_RECENT_MODELS);
    }

    let dest = dir.join(RECENT_FILENAME);
    let json_bytes = serde_json::to_vec_pretty(&models)?;

    // Atomic write via tempfile in the same directory
    let mut temp = tempfile::Builder::new()
        .prefix("model-recent-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    temp.write_all(&json_bytes)?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.persist(&dest)?;

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleDirection {
    Next,
    Prev,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CycleResult {
    Switch(String),
    NoOtherRecent,
}

pub fn cycle(recent: &[String], current: Option<&str>, direction: CycleDirection) -> CycleResult {
    let mut deduped: Vec<&str> = Vec::new();
    for id in recent {
        if !deduped.contains(&id.as_str()) {
            deduped.push(id.as_str());
        }
    }

    let distinct_count = if let Some(curr) = current {
        if deduped.contains(&curr) {
            deduped.len()
        } else {
            deduped.len() + 1
        }
    } else {
        deduped.len()
    };

    if distinct_count < 2 || deduped.is_empty() {
        return CycleResult::NoOtherRecent;
    }

    if let Some(curr) = current
        && let Some(idx) = deduped.iter().position(|&id| id == curr)
    {
        let next_idx = match direction {
            CycleDirection::Next => (idx + 1) % deduped.len(),
            CycleDirection::Prev => (idx + deduped.len() - 1) % deduped.len(),
        };
        return CycleResult::Switch(deduped[next_idx].to_string());
    }

    match direction {
        CycleDirection::Next => CycleResult::Switch(deduped[0].to_string()),
        CycleDirection::Prev => CycleResult::Switch(deduped[deduped.len() - 1].to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_from_non_existent_dir_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = load_from(temp.path());
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_load_from_garbled_file_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join(RECENT_FILENAME);
        std::fs::write(&file_path, "not a valid json array {!}").unwrap();
        let loaded = load_from(temp.path());
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_record_in_creates_file_and_maintains_mru() {
        let temp = tempfile::tempdir().unwrap();
        record_in(temp.path(), "model-a").unwrap();
        assert_eq!(load_from(temp.path()), vec!["model-a".to_string()]);

        record_in(temp.path(), "model-b").unwrap();
        assert_eq!(
            load_from(temp.path()),
            vec!["model-b".to_string(), "model-a".to_string()]
        );
    }

    #[test]
    fn test_record_in_trims_id() {
        let temp = tempfile::tempdir().unwrap();
        record_in(temp.path(), "  claude-opus-5-5  ").unwrap();
        assert_eq!(load_from(temp.path()), vec!["claude-opus-5-5".to_string()]);
    }

    #[test]
    fn test_record_in_dedup_moves_to_front() {
        let temp = tempfile::tempdir().unwrap();
        record_in(temp.path(), "model-a").unwrap();
        record_in(temp.path(), "model-b").unwrap();
        record_in(temp.path(), "model-c").unwrap();
        assert_eq!(
            load_from(temp.path()),
            vec![
                "model-c".to_string(),
                "model-b".to_string(),
                "model-a".to_string()
            ]
        );

        // Re-record model-a -> should move to front, no duplicate
        record_in(temp.path(), "model-a").unwrap();
        assert_eq!(
            load_from(temp.path()),
            vec![
                "model-a".to_string(),
                "model-c".to_string(),
                "model-b".to_string()
            ]
        );
    }

    #[test]
    fn test_record_in_caps_at_max_10() {
        let temp = tempfile::tempdir().unwrap();
        for i in 1..=15 {
            record_in(temp.path(), &format!("model-{i}")).unwrap();
        }
        let loaded = load_from(temp.path());
        assert_eq!(loaded.len(), 10);
        assert_eq!(loaded[0], "model-15");
        assert_eq!(loaded[9], "model-6");
    }

    #[test]
    fn test_cycle_models() {
        let recent = vec!["m1".to_string(), "m2".to_string(), "m3".to_string()];

        // Next from m1 -> m2
        assert_eq!(
            cycle(&recent, Some("m1"), CycleDirection::Next),
            CycleResult::Switch("m2".to_string())
        );
        // Prev from m1 -> m3 (wrapping)
        assert_eq!(
            cycle(&recent, Some("m1"), CycleDirection::Prev),
            CycleResult::Switch("m3".to_string())
        );
        // Next from m3 -> m1 (wrapping)
        assert_eq!(
            cycle(&recent, Some("m3"), CycleDirection::Next),
            CycleResult::Switch("m1".to_string())
        );
        // Current not in recent: F2 picks recent[0]
        assert_eq!(
            cycle(&recent, Some("m0"), CycleDirection::Next),
            CycleResult::Switch("m1".to_string())
        );
        // Current not in recent: shift+F2 picks recent.last()
        assert_eq!(
            cycle(&recent, Some("m0"), CycleDirection::Prev),
            CycleResult::Switch("m3".to_string())
        );

        // Fewer than 2 distinct options
        assert_eq!(
            cycle(&[], Some("m1"), CycleDirection::Next),
            CycleResult::NoOtherRecent
        );
        assert_eq!(
            cycle(&["m1".to_string()], Some("m1"), CycleDirection::Next),
            CycleResult::NoOtherRecent
        );
        assert_eq!(
            cycle(&["m1".to_string()], None, CycleDirection::Next),
            CycleResult::NoOtherRecent
        );

        // 2 distinct options (current not in recent, recent has 1 differing item)
        assert_eq!(
            cycle(&["m1".to_string()], Some("m2"), CycleDirection::Next),
            CycleResult::Switch("m1".to_string())
        );
    }
}
