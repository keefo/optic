//! Generic "durable source of truth + fast tmpfs mirror" file pair
//! (design doc §2.1, `docs/optic-daemon-scheduler.md`): writes go to a
//! real, persistent path first, then update a tmpfs cache; frequent reads
//! use only the cache, never the durable path directly. Used for
//! `config.json` (this slice) and, once wired into the scheduler actor,
//! `schedule_run_state.json`.

use std::path::Path;

use tokio::fs;

/// Reads `durable_path` once (typically at daemon startup) and writes its
/// content to `cache_path`, creating the cache's parent directory if
/// needed. If `durable_path` doesn't exist yet (fresh install), leaves the
/// cache untouched — callers already treat "no cache file" as "use
/// defaults," so there's nothing useful to seed it with.
pub async fn hydrate_cache(durable_path: &Path, cache_path: &Path) -> std::io::Result<()> {
    let content = match fs::read_to_string(durable_path).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    write_atomic(cache_path, &content).await
}

/// Writes `content` to `durable_path` first, then to `cache_path` —
/// durable first, so a crash between the two writes never leaves the
/// cache ahead of a value that was never actually durably committed
/// (design doc §2.1).
pub async fn write_through(
    durable_path: &Path,
    cache_path: &Path,
    content: &str,
) -> std::io::Result<()> {
    write_atomic(durable_path, content).await?;
    write_atomic(cache_path, content).await
}

/// Reads the fast tmpfs mirror only — never touches the durable path.
/// Every frequent/polled read (e.g. `GET /api/status`) should use this.
pub async fn read_cached(cache_path: &Path) -> std::io::Result<String> {
    fs::read_to_string(cache_path).await
}

async fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let temp_path = Path::new(&format!("{}.tmp", path.display())).to_owned();
    fs::write(&temp_path, content).await?;
    fs::rename(temp_path, path).await
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "optic-durable-state-test-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn hydrate_cache_copies_durable_content_into_a_fresh_cache_dir() {
        let durable_dir = unique_temp_dir("hydrate-durable");
        let cache_dir = unique_temp_dir("hydrate-cache");
        let durable_path = durable_dir.join("config.json");
        let cache_path = cache_dir.join("nested").join("config.json");
        tokio::fs::write(&durable_path, "hello").await.unwrap();

        hydrate_cache(&durable_path, &cache_path).await.unwrap();

        assert_eq!(read_cached(&cache_path).await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn hydrate_cache_is_a_no_op_when_durable_is_missing() {
        let durable_dir = unique_temp_dir("hydrate-missing-durable");
        let cache_dir = unique_temp_dir("hydrate-missing-cache");
        let durable_path = durable_dir.join("config.json");
        let cache_path = cache_dir.join("config.json");

        hydrate_cache(&durable_path, &cache_path).await.unwrap();

        assert!(!cache_path.exists());
    }

    #[tokio::test]
    async fn write_through_updates_both_durable_and_cache() {
        let durable_dir = unique_temp_dir("write-through-durable");
        let cache_dir = unique_temp_dir("write-through-cache");
        let durable_path = durable_dir.join("config.json");
        let cache_path = cache_dir.join("config.json");

        write_through(&durable_path, &cache_path, "v1")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&durable_path).await.unwrap(),
            "v1"
        );
        assert_eq!(read_cached(&cache_path).await.unwrap(), "v1");

        write_through(&durable_path, &cache_path, "v2")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&durable_path).await.unwrap(),
            "v2"
        );
        assert_eq!(read_cached(&cache_path).await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn read_cached_never_touches_the_durable_path() {
        let durable_dir = unique_temp_dir("isolation-durable");
        let cache_dir = unique_temp_dir("isolation-cache");
        let durable_path = durable_dir.join("config.json");
        let cache_path = cache_dir.join("config.json");
        // Deliberately write a *different* value directly to the cache,
        // without ever writing the durable path, to prove `read_cached`
        // is sourced purely from the cache.
        tokio::fs::create_dir_all(&cache_dir).await.unwrap();
        tokio::fs::write(&cache_path, "cache-only").await.unwrap();

        assert!(!durable_path.exists());
        assert_eq!(read_cached(&cache_path).await.unwrap(), "cache-only");
    }
}
