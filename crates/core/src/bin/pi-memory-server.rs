//! pi-memory resident server: stdio JSON-RPC entry point.
//!
//! Single process, multiple databases: one DB file per projectScope (physical isolation),
//! a single globally shared Embedder (the model is loaded once), and a per-project vector index.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use anyhow::Context as _;
use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::paths;
use pi_memory_core::protocol::{
    codes, methods, HelloParams, HelloResult, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use pi_memory_core::storage::{Storage, TursoStorage};

const MAX_ACTIVE_MAINTENANCE_JOBS: usize = 32;

/// Per-project runtime unit: storage + vectors (isolated per scope).
struct ProjectRuntime {
    storage: TursoStorage,
    vectors: ProjectVectors,
}

/// Pool of project runtimes; only lookup is globally locked, requests lock one project at a time.
#[derive(Clone)]
struct StoragePool {
    data_dir: PathBuf,
    projects: Arc<Mutex<HashMap<String, ProjectEntry>>>,
    open_gates: Arc<Vec<Mutex<()>>>,
    jobs: Arc<Mutex<HashMap<String, JobStatus>>>,
    next_job_id: Arc<AtomicU64>,
    project_access_clock: Arc<AtomicU64>,
}

struct ProjectEntry {
    runtime: Arc<Mutex<ProjectRuntime>>,
    last_access: u64,
}

fn evict_idle_project_runtimes(
    projects: &mut HashMap<String, ProjectEntry>,
    current_scope: &str,
    max_open: usize,
) -> usize {
    let mut evicted = 0;
    while projects.len() > max_open {
        if let Some(oldest) = projects
            .iter()
            .filter(|(scope, entry)| {
                scope.as_str() != current_scope && Arc::strong_count(&entry.runtime) == 1
            })
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(scope, _)| scope.clone())
        {
            projects.remove(&oldest);
            evicted += 1;
        } else {
            break;
        }
    }
    evicted
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JobStatus {
    job_id: String,
    kind: String,
    project_scope: String,
    state: String,
    stage: String,
    completed_items: usize,
    remaining_items: Option<i64>,
    error: Option<String>,
    details: Option<serde_json::Value>,
    started_at: String,
    updated_at: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartJobParams {
    kind: String,
    project_scope: String,
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    target_scope: Option<String>,
    #[serde(default)]
    confirmed: bool,
}

impl StoragePool {
    fn new() -> Self {
        Self {
            data_dir: paths::default_data_dir(),
            projects: Arc::new(Mutex::new(HashMap::new())),
            open_gates: Arc::new((0..64).map(|_| Mutex::new(())).collect()),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            next_job_id: Arc::new(AtomicU64::new(1)),
            project_access_clock: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Get (or lazily open) the runtime for a project.
    fn get_or_open(&self, project_scope: &str) -> anyhow::Result<Arc<Mutex<ProjectRuntime>>> {
        let access = self
            .project_access_clock
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut projects = self.lock_projects();
            if let Some(entry) = projects.get_mut(project_scope) {
                entry.last_access = access;
                return Ok(Arc::clone(&entry.runtime));
            }
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        project_scope.hash(&mut hasher);
        let open_gate = &self.open_gates[(hasher.finish() as usize) % self.open_gates.len()];
        let _open_guard = match open_gate.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                open_gate.clear_poison();
                pi_memory_core::debug_log("project open mutex poisoned; recovering guard");
                poisoned.into_inner()
            }
        };
        {
            let mut projects = self.lock_projects();
            if let Some(entry) = projects.get_mut(project_scope) {
                entry.last_access = access;
                return Ok(Arc::clone(&entry.runtime));
            }
        }
        // Opening SQLite and hydrating the vector index can be expensive. Do that outside
        // the pool mutex so another project can continue to open and serve concurrently.
        let db_path = paths::ensure_project_db_dir(&self.data_dir, project_scope)?;
        let storage = TursoStorage::open_deferred(db_path.to_str().expect("path must be UTF-8"))?;
        let cleanup_result = storage
            .purge_expired(project_scope, &pi_memory_core::runtime::now_iso())
            .and_then(|_| storage.purge_orphan_embeddings().map(|_| ()));
        let mut vectors = ProjectVectors::new()?;
        match cleanup_result.and_then(|()| vectors.hydrate(&storage)) {
            Ok(()) => storage.mark_vector_rehydrate_ready()?,
            Err(error) => {
                pi_memory_core::debug_log(&format!(
                    "startup cleanup or vector hydration failed; serving FTS-only until rebuild: {error}"
                ));
                storage.mark_vector_rehydrate_failed(&error.to_string())?;
                vectors = ProjectVectors::new()?;
            }
        }
        let candidate = Arc::new(Mutex::new(ProjectRuntime { storage, vectors }));
        let mut projects = self.lock_projects();
        if let Some(entry) = projects.get_mut(project_scope) {
            entry.last_access = access;
            return Ok(Arc::clone(&entry.runtime));
        }
        pi_memory_core::debug_log(&format!("opened project db: {db_path:?}"));
        projects.insert(
            project_scope.to_string(),
            ProjectEntry {
                runtime: Arc::clone(&candidate),
                last_access: access,
            },
        );
        let max_open = std::env::var("PI_MEMORY_MAX_OPEN_PROJECTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(16)
            .clamp(1, 256);
        let evicted = evict_idle_project_runtimes(&mut projects, project_scope, max_open);
        for _ in 0..evicted {
            pi_memory_core::debug_log("evicted least-recently-used project runtime");
        }
        if projects.len() > max_open {
            pi_memory_core::debug_log(
                "project runtime cache exceeds configured limit because all candidates are in use",
            );
        }
        drop(projects);
        // Retryable, lease-fenced migration; this connection is independent of request locks.
        let migration_path = db_path.to_string_lossy().into_owned();
        std::thread::spawn(move || {
            let storage = (0..3).find_map(|open_attempt| {
                match TursoStorage::open_deferred(&migration_path) {
                    Ok(storage) => Some(storage),
                    Err(error) if open_attempt < 2 => {
                        pi_memory_core::debug_log(&format!(
                            "background FTS migration open failed; retrying: {error}"
                        ));
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        None
                    }
                    Err(error) => {
                        pi_memory_core::debug_log(&format!(
                            "background FTS migration unavailable: {error}"
                        ));
                        None
                    }
                }
            });
            let Some(storage) = storage else { return };

            let mut failures = 0_u32;
            let mut delay = std::time::Duration::from_millis(500);
            loop {
                match storage.migrate_search_text_if_needed() {
                    Ok(true) => return,
                    Ok(false) => {
                        // Another process owns the migration lease. Keep watching so a crashed
                        // owner can be replaced when its 30-second lease expires.
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        failures = 0;
                        delay = std::time::Duration::from_millis(500);
                    }
                    Err(error) => {
                        failures += 1;
                        pi_memory_core::debug_log(&format!(
                            "background FTS migration failed (attempt {failures}): {error}"
                        ));
                        if failures >= 3 {
                            pi_memory_core::debug_log(
                                "background FTS migration deferred after repeated errors",
                            );
                            return;
                        }
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(std::time::Duration::from_secs(5));
                    }
                }
            }
        });
        Ok(candidate)
    }

    /// Number of open projects (diagnostics).
    fn open_project_count(&self) -> usize {
        self.lock_projects().len()
    }

    fn new_consolidation_claim_token(&self) -> String {
        let sequence = self
            .next_job_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        std::process::id().hash(&mut hasher);
        sequence.hash(&mut hasher);
        timestamp.hash(&mut hasher);
        format!("claim:{:016x}", hasher.finish())
    }

    fn start_job(
        &self,
        params: StartJobParams,
        shared: SharedEmbedder,
    ) -> anyhow::Result<JobStatus> {
        match params.kind.as_str() {
            "prepareModel" | "rebuildEmbeddings" | "rebuildFts" => {}
            "clearAll" if params.confirmed => {}
            "clearAll" => anyhow::bail!("clearAll job requires explicit confirmation"),
            "clearProject"
                if params
                    .target_scope
                    .as_deref()
                    .is_some_and(|scope| !scope.is_empty()) => {}
            "clearProject" => anyhow::bail!("clearProject job requires targetScope"),
            "purgeProject"
                if params
                    .target_scope
                    .as_deref()
                    .is_some_and(|scope| !scope.is_empty()) => {}
            "purgeProject" => anyhow::bail!("purgeProject job requires targetScope"),
            "migrate"
                if params
                    .source_path
                    .as_deref()
                    .is_some_and(|path| !path.is_empty()) => {}
            "migrate" => anyhow::bail!("migrate job requires sourcePath"),
            _ => anyhow::bail!("unsupported job kind: {}", params.kind),
        }
        let sequence = self
            .next_job_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let job_id = format!("{}:{sequence}", std::process::id());
        let now = pi_memory_core::runtime::now_iso();
        let job = JobStatus {
            job_id: job_id.clone(),
            kind: params.kind.clone(),
            project_scope: params.project_scope.clone(),
            state: "queued".into(),
            stage: "queued".into(),
            completed_items: 0,
            remaining_items: None,
            error: None,
            details: None,
            started_at: now.clone(),
            updated_at: now,
        };
        {
            let mut jobs = self.lock_jobs();
            let active_jobs = jobs
                .values()
                .filter(|job| job.state == "queued" || job.state == "running")
                .count();
            anyhow::ensure!(
                active_jobs < MAX_ACTIVE_MAINTENANCE_JOBS,
                "maintenance job limit reached ({MAX_ACTIVE_MAINTENANCE_JOBS} active jobs)"
            );
            if jobs.len() >= 256 {
                let mut terminal = jobs
                    .values()
                    .filter(|job| job.state == "completed" || job.state == "failed")
                    .map(|job| (job.updated_at.clone(), job.job_id.clone()))
                    .collect::<Vec<_>>();
                terminal.sort();
                for (_, old_id) in terminal.into_iter().take(jobs.len().saturating_sub(255)) {
                    jobs.remove(&old_id);
                }
            }
            jobs.insert(job_id.clone(), job.clone());
        }
        let pool = self.clone();
        let worker_job_id = job_id.clone();
        std::thread::spawn(move || {
            let worker_pool = pool.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool.run_job(job_id, params, shared)
            }))
            .is_err()
            {
                worker_pool.update_job(
                    &worker_job_id,
                    "failed",
                    "worker_panicked",
                    0,
                    None,
                    Some("maintenance worker panicked".into()),
                );
            }
        });
        Ok(job)
    }

    fn run_job(&self, job_id: String, params: StartJobParams, shared: SharedEmbedder) {
        self.update_job(&job_id, "running", "starting", 0, None, None);
        let result = match params.kind.as_str() {
            "prepareModel" => self.run_prepare_model_job(&job_id, &shared),
            "migrate" => self.run_migrate_job(&job_id, &params),
            "rebuildEmbeddings" => {
                self.run_rebuild_embeddings_job(&job_id, &params.project_scope, &shared)
            }
            "rebuildFts" => self.run_rebuild_fts_job(&job_id, &params.project_scope),
            "clearProject" => self.run_clear_project_job(
                &job_id,
                params
                    .target_scope
                    .as_deref()
                    .unwrap_or(&params.project_scope),
            ),
            "purgeProject" => self.run_purge_project_job(
                &job_id,
                params
                    .target_scope
                    .as_deref()
                    .unwrap_or(&params.project_scope),
            ),
            "clearAll" => self.run_clear_all_job(&job_id),
            _ => Err(anyhow::anyhow!("unsupported job kind: {}", params.kind)),
        };
        match result {
            Ok((count, remaining, stage)) => {
                self.update_job(&job_id, "completed", stage, count, remaining, None)
            }
            Err(error) => self.update_job(
                &job_id,
                "failed",
                "failed",
                0,
                None,
                Some(error.to_string()),
            ),
        }
    }

    fn run_prepare_model_job(
        &self,
        job_id: &str,
        shared: &SharedEmbedder,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        self.update_job(job_id, "running", "downloading", 0, None, None);
        let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
        pi_memory_core::model_download::ensure_model_dir(&model_dir)?;
        self.update_job(job_id, "running", "loading", 0, None, None);
        shared.warmup();
        shared.wait_loading(std::time::Duration::from_secs(300));
        anyhow::ensure!(
            shared.ensure_loaded(),
            "embedding model did not become ready: {}",
            shared.status()
        );
        Ok((0, Some(0), "ready"))
    }

    fn run_migrate_job(
        &self,
        job_id: &str,
        params: &StartJobParams,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        let source_path = params
            .source_path
            .as_deref()
            .context("migrate job requires sourcePath")?;
        self.update_job(job_id, "running", "opening", 0, None, None);
        // Import through a dedicated connection so the project's runtime mutex is not held
        // across a large SQLite copy. Take the runtime lock only to refresh its vector index.
        let _project = self.get_or_open(&params.project_scope)?;
        let db_path = paths::ensure_project_db_dir(&self.data_dir, &params.project_scope)?;
        let migration_storage = TursoStorage::open_deferred(
            db_path.to_str().context("project DB path must be UTF-8")?,
        )?;
        self.update_job(job_id, "running", "importing", 0, None, None);
        let stats =
            migration_storage.migrate_from_scope(source_path, Some(&params.project_scope))?;
        let fts_dirty = stats
            .get("fts_dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        self.update_job(job_id, "running", "rehydrating", 0, None, None);
        let project = self.get_or_open(&params.project_scope)?;
        let mut runtime = lock_project(&project);
        let ProjectRuntime { storage, vectors } = &mut *runtime;
        let imported = stats
            .get("working")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        match rehydrate_project_vectors(storage, vectors, !fts_dirty) {
            Ok(()) if fts_dirty => {
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "partial",
                        "ftsDirty": true,
                        "needsRehydrate": false,
                        "targetScope": params.project_scope,
                        "imported": stats,
                    }),
                );
                Ok((imported, Some(1), "imported_fts_partial"))
            }
            Ok(()) => {
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "ok",
                        "targetScope": params.project_scope,
                        "imported": stats,
                    }),
                );
                Ok((imported, Some(0), "imported"))
            }
            Err(error) => {
                pi_memory_core::debug_log(&format!("migration vector rehydrate deferred: {error}"));
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "partial",
                        "needsRehydrate": true,
                        "targetScope": params.project_scope,
                        "imported": stats,
                        "error": error.to_string(),
                    }),
                );
                Ok((imported, Some(1), "imported_vectors_partial"))
            }
        }
    }

    fn run_rebuild_embeddings_job(
        &self,
        job_id: &str,
        project_scope: &str,
        shared: &SharedEmbedder,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        anyhow::ensure!(
            shared.ensure_loaded(),
            "embedding model is not ready; run prepare-model first"
        );
        let project = self.get_or_open(project_scope)?;
        let mut completed = 0usize;
        loop {
            let progress = {
                let mut runtime = lock_project(&project);
                let ProjectRuntime { storage, vectors } = &mut *runtime;
                pi_memory_core::runtime::backfill_embeddings(storage, shared, vectors, 32)?
            };
            completed += progress.embedded;
            self.update_job(
                job_id,
                "running",
                "embedding",
                completed,
                Some(progress.remaining),
                None,
            );
            if progress.remaining == 0 {
                return Ok((completed, Some(0), "rebuilt"));
            }
            if progress.embedded == 0 {
                if progress.skipped_empty > 0 {
                    return Ok((completed, Some(progress.remaining), "empty_text_skipped"));
                }
                anyhow::bail!(
                    "embedding backfill made no progress; {} rows remain",
                    progress.remaining
                );
            }
        }
    }

    fn run_rebuild_fts_job(
        &self,
        job_id: &str,
        project_scope: &str,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        self.update_job(job_id, "running", "opening", 0, None, None);
        let project = self.get_or_open(project_scope)?;
        self.update_job(job_id, "running", "rebuilding_fts", 0, None, None);
        let mut runtime = lock_project(&project);
        let ProjectRuntime { storage, vectors } = &mut *runtime;
        rebuild_project_indexes(storage, vectors)?;
        self.set_job_details(
            job_id,
            serde_json::json!({
                "dbCommitted": true,
                "indexState": "ok",
                "targetScope": project_scope,
            }),
        );
        Ok((0, Some(0), "rebuilt"))
    }

    fn run_clear_project_job(
        &self,
        job_id: &str,
        project_scope: &str,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        self.update_job(job_id, "running", "opening", 0, None, None);
        let project = self.get_or_open(project_scope)?;
        let mut runtime = lock_project(&project);
        let ProjectRuntime { storage, vectors } = &mut *runtime;
        self.update_job(job_id, "running", "deleting", 0, None, None);
        storage.clear_project_memories(project_scope)?;
        self.update_job(job_id, "running", "rebuilding_indexes", 0, None, None);
        match rebuild_project_indexes(storage, vectors) {
            Ok(()) => {
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "ok",
                        "targetScope": project_scope,
                    }),
                );
                Ok((0, Some(0), "cleared"))
            }
            Err(error) => {
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "partial",
                        "ftsDirty": true,
                        "needsRehydrate": true,
                        "targetScope": project_scope,
                        "error": error.to_string(),
                    }),
                );
                Ok((0, Some(1), "cleared_indexes_partial"))
            }
        }
    }

    fn run_purge_project_job(
        &self,
        job_id: &str,
        project_scope: &str,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        self.update_job(job_id, "running", "opening", 0, None, None);
        let project = self.get_or_open(project_scope)?;
        let mut runtime = lock_project(&project);
        let ProjectRuntime { storage, vectors } = &mut *runtime;
        self.update_job(job_id, "running", "purging_expired", 0, None, None);
        let removed = storage.purge_expired(project_scope, &pi_memory_core::runtime::now_iso())?;
        let orphan_embeddings = match storage.purge_orphan_embeddings() {
            Ok(count) => count,
            Err(error) => {
                let _ = storage.mark_vector_rehydrate_failed(&error.to_string());
                if let Ok(empty) = ProjectVectors::new() {
                    *vectors = empty;
                }
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "partial",
                        "needsRehydrate": true,
                        "targetScope": project_scope,
                        "removed": removed,
                        "error": error.to_string(),
                    }),
                );
                return Ok((removed, Some(1), "purged_partial"));
            }
        };
        self.update_job(
            job_id,
            "running",
            "rehydrating_vectors",
            removed,
            None,
            None,
        );
        match rehydrate_project_vectors(storage, vectors, false) {
            Ok(()) => {
                let fts_dirty = match storage.fts_rebuild_needed() {
                    Ok(dirty) => dirty,
                    Err(error) => {
                        let _ = storage.mark_vector_rehydrate_failed(&error.to_string());
                        if let Ok(empty) = ProjectVectors::new() {
                            *vectors = empty;
                        }
                        self.set_job_details(
                            job_id,
                            serde_json::json!({
                                "dbCommitted": true,
                                "indexState": "partial",
                                "needsRehydrate": true,
                                "targetScope": project_scope,
                                "removed": removed,
                                "orphanEmbeddings": orphan_embeddings,
                                "error": error.to_string(),
                            }),
                        );
                        return Ok((removed, Some(1), "purged_state_partial"));
                    }
                };
                let index_state = if fts_dirty { "partial" } else { "ok" };
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": index_state,
                        "ftsDirty": fts_dirty,
                        "needsRehydrate": false,
                        "targetScope": project_scope,
                        "removed": removed,
                        "orphanEmbeddings": orphan_embeddings,
                    }),
                );
                if fts_dirty {
                    Ok((removed, Some(1), "purged_fts_partial"))
                } else {
                    Ok((removed, Some(0), "purged"))
                }
            }
            Err(error) => {
                self.set_job_details(
                    job_id,
                    serde_json::json!({
                        "dbCommitted": true,
                        "indexState": "partial",
                        "needsRehydrate": true,
                        "targetScope": project_scope,
                        "removed": removed,
                        "orphanEmbeddings": orphan_embeddings,
                        "error": error.to_string(),
                    }),
                );
                Ok((removed, Some(1), "purged_vectors_partial"))
            }
        }
    }

    fn run_clear_all_job(
        &self,
        job_id: &str,
    ) -> anyhow::Result<(usize, Option<i64>, &'static str)> {
        self.update_job(job_id, "running", "clearing_projects", 0, None, None);
        let report = self.clear_all_memories()?;
        let scopes = report
            .get("scopes")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let partial = scopes.iter().any(|scope| {
            scope
                .get("db_committed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
                || scope.get("index_state").and_then(serde_json::Value::as_str) != Some("ok")
        });
        let count = scopes.len();
        self.set_job_details(job_id, report);
        if partial {
            Ok((count, Some(1), "cleared_all_partial"))
        } else {
            Ok((count, Some(0), "cleared_all"))
        }
    }

    fn job_status(&self, job_id: &str) -> Option<JobStatus> {
        self.lock_jobs().get(job_id).cloned()
    }

    fn update_job(
        &self,
        job_id: &str,
        state: &str,
        stage: &str,
        completed_items: usize,
        remaining_items: Option<i64>,
        error: Option<String>,
    ) {
        if let Some(job) = self.lock_jobs().get_mut(job_id) {
            job.state = state.into();
            job.stage = stage.into();
            job.completed_items = completed_items;
            job.remaining_items = remaining_items;
            job.error = error;
            job.updated_at = pi_memory_core::runtime::now_iso();
        }
    }

    fn set_job_details(&self, job_id: &str, details: serde_json::Value) {
        if let Some(job) = self.lock_jobs().get_mut(job_id) {
            job.details = Some(details);
            job.updated_at = pi_memory_core::runtime::now_iso();
        }
    }

    fn lock_jobs(&self) -> MutexGuard<'_, HashMap<String, JobStatus>> {
        match self.jobs.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.jobs.clear_poison();
                pi_memory_core::debug_log("job status mutex poisoned; recovering job map");
                poisoned.into_inner()
            }
        }
    }

    fn backfill_open_projects(&self, shared: &SharedEmbedder) {
        let projects = self
            .lock_projects()
            .values()
            .map(|entry| Arc::clone(&entry.runtime))
            .collect::<Vec<_>>();
        for project in projects {
            loop {
                let result = {
                    let mut runtime = lock_project(&project);
                    let ProjectRuntime { storage, vectors } = &mut *runtime;
                    pi_memory_core::runtime::backfill_embeddings(storage, shared, vectors, 32)
                };
                match result {
                    Ok(result) if result.remaining == 0 || result.embedded == 0 => break,
                    Ok(_) => {}
                    Err(error) => {
                        pi_memory_core::debug_log(&format!("embedding backfill failed: {error}"));
                        break;
                    }
                }
            }
        }
    }

    fn lock_projects(&self) -> MutexGuard<'_, HashMap<String, ProjectEntry>> {
        match self.projects.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.projects.clear_poison();
                pi_memory_core::debug_log("project map mutex poisoned; recovering map guard");
                poisoned.into_inner()
            }
        }
    }

    fn list_project_scopes(&self) -> anyhow::Result<Vec<String>> {
        use base64::Engine as _;
        let mut scopes = self
            .lock_projects()
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let root = self.data_dir.join("memories");
        if root.is_dir() {
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let scope = if name == "root" {
                    String::new()
                } else {
                    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&name)
                    else {
                        continue;
                    };
                    let Ok(scope) = String::from_utf8(bytes) else {
                        continue;
                    };
                    scope
                };
                if paths::normalize_project_scope(&scope) == name
                    && paths::resolve_project_db_path(&self.data_dir, &scope).is_file()
                {
                    scopes.insert(scope);
                }
            }
        }
        let mut scopes = scopes.into_iter().collect::<Vec<_>>();
        scopes.sort();
        Ok(scopes)
    }

    fn clear_all_memories(&self) -> anyhow::Result<serde_json::Value> {
        let database_scopes = self.list_project_scopes()?;
        let mut results = Vec::new();
        for database_scope in database_scopes {
            let project = match self.get_or_open(&database_scope) {
                Ok(project) => project,
                Err(error) => {
                    results.push(serde_json::json!({
                        "database_scope": database_scope,
                        "project_scope": database_scope,
                        "db_committed": false,
                        "index_state": "partial",
                        "error": error.to_string(),
                    }));
                    continue;
                }
            };
            let mut runtime = lock_project(&project);
            let ProjectRuntime { storage, vectors } = &mut *runtime;
            let mut scopes = match storage.list_project_scopes() {
                Ok(scopes) => scopes
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>(),
                Err(error) => {
                    results.push(serde_json::json!({
                        "database_scope": database_scope,
                        "project_scope": database_scope,
                        "db_committed": false,
                        "index_state": "partial",
                        "error": format!("failed to list scopes: {error}"),
                    }));
                    continue;
                }
            };
            // Include the database's owning scope even when it has no current rows, so a
            // stale runtime/index state is still reset by clear all.
            scopes.insert(database_scope.clone());
            let mut committed_indices = Vec::new();
            for scope in scopes {
                match storage.clear_project_memories(&scope) {
                    Ok(()) => {
                        committed_indices.push(results.len());
                        results.push(serde_json::json!({
                            "database_scope": database_scope,
                            "project_scope": scope,
                            "db_committed": true,
                            "index_state": "pending",
                        }));
                    }
                    Err(error) => {
                        results.push(serde_json::json!({
                            "database_scope": database_scope,
                            "project_scope": scope,
                            "db_committed": false,
                            "index_state": "partial",
                            "error": error.to_string(),
                        }));
                    }
                }
            }
            if committed_indices.is_empty() {
                continue;
            }
            if let Err(error) = storage.purge_orphan_embeddings() {
                let _ = storage.mark_vector_rehydrate_failed(&error.to_string());
                if let Ok(empty) = ProjectVectors::new() {
                    *vectors = empty;
                }
                for index in committed_indices {
                    results[index]["index_state"] = serde_json::json!("partial");
                    results[index]["needs_rehydrate"] = serde_json::json!(true);
                    results[index]["error"] =
                        serde_json::json!(format!("orphan embedding cleanup failed: {error}"));
                }
                continue;
            }
            match rebuild_project_indexes(storage, vectors) {
                Ok(()) => {
                    for index in committed_indices {
                        results[index]["index_state"] = serde_json::json!("ok");
                    }
                }
                Err(error) => {
                    for index in committed_indices {
                        results[index]["index_state"] = serde_json::json!("partial");
                        results[index]["fts_dirty"] = serde_json::json!(true);
                        results[index]["needs_rehydrate"] = serde_json::json!(true);
                        results[index]["error"] = serde_json::json!(error.to_string());
                    }
                }
            }
        }
        Ok(serde_json::json!({
            "operation": "clear_all",
            "confirmed": true,
            "scopes": results,
        }))
    }
}

/// Recover a per-project runtime after a panic. Roll back any open DB transaction and
/// reconstruct the in-memory vector index from the durable database before serving again.
fn lock_project(project: &Mutex<ProjectRuntime>) -> MutexGuard<'_, ProjectRuntime> {
    match project.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            project.clear_poison();
            let mut runtime = poisoned.into_inner();
            pi_memory_core::debug_log(
                "project runtime mutex poisoned; rolling back and rehydrating",
            );
            let _ = runtime.storage.rollback_tx();
            let ProjectRuntime { storage, vectors } = &mut *runtime;
            match vectors.hydrate(storage) {
                Ok(()) => {
                    let _ = storage.mark_vector_rehydrate_ready();
                }
                Err(error) => {
                    pi_memory_core::debug_log(&format!("vector recovery failed: {error}"));
                    let _ = storage.mark_vector_rehydrate_failed(&error.to_string());
                    if let Ok(empty_vectors) = ProjectVectors::new() {
                        *vectors = empty_vectors;
                    }
                }
            }
            runtime
        }
    }
}

/// Refresh an in-memory vector index after a committed DB mutation. On failure, persist the
/// partial state and replace the live index with an empty one so stale vectors cannot be served.
fn rehydrate_project_vectors(
    storage: &TursoStorage,
    vectors: &mut ProjectVectors,
    mark_fts_ready: bool,
) -> anyhow::Result<()> {
    match vectors.hydrate(storage) {
        Ok(()) if mark_fts_ready => storage.mark_indexes_ready(),
        Ok(()) => storage.mark_vector_rehydrate_ready(),
        Err(error) => {
            storage.mark_vector_rehydrate_failed(&error.to_string())?;
            if let Ok(empty) = ProjectVectors::new() {
                *vectors = empty;
            }
            Err(error)
        }
    }
}

/// Clear commits DB rows and a durable partial marker; rebuild FTS before declaring the
/// derived indexes healthy. On FTS failure, empty the live vector index so deleted memories
/// cannot be served while the durable marker waits for recovery.
fn rebuild_project_indexes(
    storage: &TursoStorage,
    vectors: &mut ProjectVectors,
) -> anyhow::Result<()> {
    if let Err(error) = storage.rebuild_fts() {
        storage.mark_vector_rehydrate_failed(&error.to_string())?;
        if let Ok(empty) = ProjectVectors::new() {
            *vectors = empty;
        }
        return Err(error);
    }
    rehydrate_project_vectors(storage, vectors, true)
}

static MODEL_WARMUP_IN_PROGRESS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

struct ModelWarmupRegistration(PathBuf);

impl Drop for ModelWarmupRegistration {
    fn drop(&mut self) {
        let registry = MODEL_WARMUP_IN_PROGRESS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut active = match registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                registry.clear_poison();
                poisoned.into_inner()
            }
        };
        active.remove(&self.0);
    }
}

fn model_retry_delay(attempt: u32) -> std::time::Duration {
    let seconds = 1_u64
        .checked_shl(attempt.saturating_sub(1).min(9))
        .unwrap_or(512)
        .min(300);
    std::time::Duration::from_secs(seconds)
}

fn start_model_warmup(shared: &SharedEmbedder, pool: &StoragePool) {
    if let Err(error) = pi_memory_core::model_manifest::validate_verification_config() {
        pi_memory_core::debug_log(&format!(
            "model verification configuration rejected: {error}"
        ));
        return;
    }
    if shared.ensure_loaded() {
        return;
    }
    let shared = shared.clone();
    let pool = pool.clone();
    let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
    let registry = MODEL_WARMUP_IN_PROGRESS.get_or_init(|| Mutex::new(HashSet::new()));
    let registered = {
        let mut active = match registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                registry.clear_poison();
                poisoned.into_inner()
            }
        };
        active.insert(model_dir.clone())
    };
    if !registered {
        return;
    }
    let retry_key = model_dir.clone();
    let worker = std::thread::Builder::new()
        .name("pi-memory-model-warmup".into())
        .spawn(move || {
            let _registration = ModelWarmupRegistration(model_dir.clone());
            let mut attempts = 0_u32;
            loop {
                if !shared.model_ready() {
                    if let Err(error) = pi_memory_core::model_download::ensure_model_dir(&model_dir)
                    {
                        attempts = attempts.saturating_add(1);
                        let delay = model_retry_delay(attempts);
                        pi_memory_core::debug_log(&format!(
                            "background model download failed (retry in {}s): {error}",
                            delay.as_secs()
                        ));
                        std::thread::sleep(delay);
                        continue;
                    }
                    pi_memory_core::debug_log(&format!(
                        "model downloaded in background: {}",
                        model_dir.display()
                    ));
                }

                shared.warmup();
                shared.wait_loading(std::time::Duration::from_secs(300));
                if shared.ensure_loaded() {
                    pool.backfill_open_projects(&shared);
                    return;
                }
                attempts = attempts.saturating_add(1);
                let delay = model_retry_delay(attempts);
                pi_memory_core::debug_log(&format!(
                    "background model load did not become ready (retry in {}s): {}",
                    delay.as_secs(),
                    shared.status()
                ));
                std::thread::sleep(delay);
            }
        });
    if let Err(error) = worker {
        let mut active = match registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                registry.clear_poison();
                poisoned.into_inner()
            }
        };
        active.remove(&retry_key);
        pi_memory_core::debug_log(&format!("could not start background model warmup: {error}"));
    }
}

fn main() {
    // 单例模式：--socket <path> 时监听 unix socket 供多个 pi 会话复用同一进程；
    // 缺省时保持 stdio 模式（测试/调试用）。
    let socket_path = std::env::args()
        .position(|a| a == "--socket")
        .and_then(|i| std::env::args().nth(i + 1));
    if let Some(path) = socket_path {
        #[cfg(unix)]
        {
            return run_socket_server(&path);
        }
        #[cfg(not(unix))]
        {
            // 非 unix 平台回退 stdio 模式（当前仅发布 darwin/linux）
            eprintln!("[pi-memory-rust] socket mode not supported here, falling back to stdio");
        }
    }

    let shared = SharedEmbedder::new();
    let mut pool = StoragePool::new();

    start_model_warmup(&shared, &pool);

    serve(&mut pool, &shared);
}

/// Socket server shares the project pool and embedder across concurrent client connections.
struct SharedState {
    pool: StoragePool,
    shared: SharedEmbedder,
}

#[cfg(unix)]
struct ConnectionCountGuard(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(unix)]
impl Drop for ConnectionCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 单例模式：监听 unix socket，多个 pi 会话的连接复用同一进程（模型只加载一份）。
/// 生命周期：显式 shutdown（且无其他连接）立即退出；空闲 60s（无任何连接）超时退出防孤儿；
/// socket 文件退出时清理。
#[cfg(unix)]
fn run_socket_server(socket_path: &str) {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // 确保 socket 父目录存在（~/.local/share/pi-memory-rust 可能未创建）
    let socket = std::path::Path::new(socket_path);
    if UnixStream::connect(socket_path).is_ok() {
        pi_memory_core::debug_log(&format!("socket server already running at {socket_path}"));
        return;
    }
    if let Some(parent) = socket.parent() {
        let parent_existed = parent.exists();
        if let Err(error) = std::fs::create_dir_all(parent) {
            eprintln!("[pi-memory-rust] failed to create socket directory: {error}");
            std::process::exit(1);
        }
        if !parent_existed
            || parent
                .file_name()
                .is_some_and(|name| name == "pi-memory-rust")
        {
            if let Err(error) =
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                eprintln!("[pi-memory-rust] failed to secure socket directory: {error}");
                std::process::exit(1);
            }
        }
    }
    // flock is released by the kernel on process exit, including crashes. Keep this
    // file in place so all contenders always lock the same inode.
    let lock_path = socket.with_extension("lock");
    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "[pi-memory-rust] failed to open socket lock {}: {error}",
                lock_path.display()
            );
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
    {
        eprintln!(
            "[pi-memory-rust] failed to secure socket lock {}: {error}",
            lock_path.display()
        );
        std::process::exit(1);
    }
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // LOCK_EX | LOCK_NB. If another server owns the lock, leave its socket untouched.
    if unsafe { flock(lock_file.as_raw_fd(), 2 | 4) } != 0 {
        if UnixStream::connect(socket_path).is_ok() {
            eprintln!("[pi-memory-rust] socket server already running at {socket_path}");
        } else {
            eprintln!("[pi-memory-rust] another server is starting for {socket_path}");
        }
        std::process::exit(1);
    }
    // Recheck after acquiring the lock: another server may have bound between the
    // initial probe and our lock attempt. Never unlink a live server's socket.
    if UnixStream::connect(socket_path).is_ok() {
        pi_memory_core::debug_log(&format!("socket server already running at {socket_path}"));
        return;
    }
    // A failed connect identifies a stale socket only when the path is actually a
    // socket. Refuse to unlink unrelated files or symlinks at the configured path.
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if let Err(error) = std::fs::remove_file(socket) {
                eprintln!("[pi-memory-rust] failed to remove stale socket {socket_path}: {error}");
                std::process::exit(1);
            }
        }
        Ok(_) => {
            eprintln!("[pi-memory-rust] refusing to replace non-socket path {socket_path}");
            std::process::exit(1);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("[pi-memory-rust] failed to inspect socket path {socket_path}: {error}");
            std::process::exit(1);
        }
    }
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[pi-memory-rust] failed to bind socket {socket_path}: {e}");
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("[pi-memory-rust] failed to secure socket {socket_path}: {error}");
        std::process::exit(1);
    }
    // 非阻塞 accept：空闲检测 + 退出判断
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("[pi-memory-rust] failed to set socket non-blocking: {e}");
        std::process::exit(1);
    }

    let state = Arc::new(SharedState {
        pool: StoragePool::new(),
        shared: SharedEmbedder::new(),
    });
    let conn_count = Arc::new(AtomicUsize::new(0));
    let exit_flag = Arc::new(AtomicBool::new(false));

    start_model_warmup(&state.shared, &state.pool);

    let idle_timeout = std::time::Duration::from_secs(60);
    let mut idle_start: Option<std::time::Instant> = None;
    pi_memory_core::debug_log(&format!("socket server listening on {socket_path}"));
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                idle_start = None; // 有连接：重置空闲计时
                conn_count.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&state);
                let conn_count = Arc::clone(&conn_count);
                let exit_flag = Arc::clone(&exit_flag);
                std::thread::spawn(move || {
                    let _connection_guard = ConnectionCountGuard(conn_count);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_socket_conn(stream, state, exit_flag);
                    }));
                    if result.is_err() {
                        pi_memory_core::debug_log("socket connection handler panicked");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if exit_flag.load(Ordering::Relaxed) && conn_count.load(Ordering::SeqCst) == 0 {
                    break; // 显式 shutdown 且无其他连接
                }
                if conn_count.load(Ordering::SeqCst) == 0 {
                    let now = std::time::Instant::now();
                    idle_start = Some(idle_start.unwrap_or(now));
                    if now.duration_since(idle_start.unwrap()) >= idle_timeout {
                        pi_memory_core::debug_log("socket server idle, exiting");
                        break; // 空闲超时：防孤儿进程
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }

    // 等待在途连接处理完（shutdown 后）
    while conn_count.load(Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // 等待后台模型加载结束再退出（避免与 ONNX 全局初始化竞态）
    state
        .shared
        .wait_loading(std::time::Duration::from_millis(800));
    let _ = std::fs::remove_file(socket_path);
}

/// 处理单个 socket 连接：读帧 → 串行执行 RPC → 写响应；shutdown 置退出标志并关闭该连接。
#[cfg(unix)]
fn handle_socket_conn(
    stream: std::os::unix::net::UnixStream,
    state: Arc<SharedState>,
    exit_flag: Arc<AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    // accept 的 stream 继承 listener 的非阻塞标志，恢复阻塞读以免 EAGAIN 误判断开
    if let Err(e) = stream.set_nonblocking(false) {
        eprintln!("[pi-memory-rust] failed to set stream blocking: {e}");
        return;
    }
    let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = std::io::BufWriter::new(stream);
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // 客户端断开
            Err(e) => {
                eprintln!("[pi-memory-rust] read failed: {e}");
                break;
            }
        };
        let request: RpcRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pi-memory-rust] request parse failed: {e}");
                continue;
            }
        };
        let shutdown = request.method == methods::SHUTDOWN;
        let payload = handle_request(&request, &state.pool, &state.shared);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] write failed: {e}");
            break;
        }
        if shutdown {
            exit_flag.store(true, Ordering::SeqCst);
            break;
        }
    }
}

fn serve(pool: &mut StoragePool, shared: &SharedEmbedder) {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // stdin EOF: host closed
            Err(e) => {
                eprintln!("[pi-memory-rust] read failed: {e}");
                break;
            }
        };
        let request: RpcRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pi-memory-rust] request parse failed: {e}");
                continue;
            }
        };
        let shutdown = request.method == methods::SHUTDOWN;
        let payload = handle_request(&request, pool, shared);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] write failed: {e}");
            break;
        }
        if shutdown {
            // Bounded wait for the background model load before exiting (avoids SIGSEGV from
            // racing ONNX global init); give up on timeout — only happens when exiting right
            // after startup, so normal shutdown stays fast
            shared.wait_loading(std::time::Duration::from_millis(800));
            break;
        }
    }
}

/// Handle a single request and return the response JSON
fn handle_request(
    request: &RpcRequest,
    pool: &StoragePool,
    shared: &SharedEmbedder,
) -> serde_json::Value {
    let response = match request.method.as_str() {
        methods::HELLO => {
            let params: HelloParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(_) => {
                    return serde_json::to_value(RpcResponse::err(
                        request.id,
                        codes::INVALID_PARAMS,
                        "invalid hello params",
                    ))
                    .expect("serialization cannot fail");
                }
            };
            if params.version != PROTOCOL_VERSION {
                RpcResponse::err(
                    request.id,
                    codes::VERSION_MISMATCH,
                    format!(
                        "protocol version mismatch: client {}, server {}",
                        params.version, PROTOCOL_VERSION
                    ),
                )
            } else {
                RpcResponse::ok(
                    request.id,
                    serde_json::to_value(HelloResult {
                        version: PROTOCOL_VERSION,
                        name: "pi-memory-rust".into(),
                    })
                    .expect("serialization cannot fail"),
                )
            }
        }
        methods::DIAGNOSE => {
            // Diagnose the current project (optional projectScope param)
            let scope = request
                .params
                .get("projectScope")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            match pool.get_or_open(scope) {
                Ok(runtime) => {
                    let runtime = lock_project(&runtime);
                    let mut info = runtime.storage.diagnose().unwrap_or_else(
                        |e| serde_json::json!({ "backend": "turso", "error": e.to_string() }),
                    );
                    info["project"] = serde_json::json!(scope);
                    info["embedding"] = serde_json::json!(shared.status());
                    info["vector_index_len"] = serde_json::json!(runtime.vectors.index_len());
                    info["last_recall"] = serde_json::to_value(
                        pi_memory_core::runtime::last_recall_diagnostics(scope),
                    )
                    .unwrap_or(serde_json::Value::Null);
                    let active_model_id = pi_memory_core::embedding_runtime::active_model_id();
                    info["embeddings_missing"] = serde_json::json!(
                        runtime
                            .storage
                            .count_missing_embeddings(
                                pi_memory_core::models::MemoryKind::Working,
                                &active_model_id
                            )
                            .unwrap_or(0)
                            + runtime
                                .storage
                                .count_missing_embeddings(
                                    pi_memory_core::models::MemoryKind::Episodic,
                                    &active_model_id
                                )
                                .unwrap_or(0)
                    );
                    info["embeddings_stale_model"] = serde_json::json!(runtime
                        .storage
                        .count_stale_model_embeddings(&active_model_id)
                        .unwrap_or(0));
                    info["unsafe_model"] = serde_json::json!(
                        std::env::var("PI_MEMORY_MODEL_VERIFY").ok().as_deref() == Some("0")
                            && (cfg!(debug_assertions)
                                || std::env::var("PI_MEMORY_ENV").ok().as_deref()
                                    == Some("development"))
                    );
                    info["open_projects"] = serde_json::json!(pool.open_project_count());
                    RpcResponse::ok(request.id, info)
                }
                Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
            }
        }
        methods::BUILD_RECALL_INJECTION => {
            match serde_json::from_value::<RecallParams>(request.params.clone()) {
                // 无信息 prompt 不进入检索，避免无查询内容时加载模型或注入记忆
                Ok(params)
                    if !pi_memory_core::text::has_informative_recall_query(&params.prompt) =>
                {
                    RpcResponse::ok(request.id, serde_json::json!(""))
                }
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let mut runtime = lock_project(&runtime);
                        let ProjectRuntime { storage, vectors } = &mut *runtime;
                        if shared.ensure_loaded() {
                            let _ = pi_memory_core::runtime::backfill_embeddings(
                                storage, shared, vectors, 4,
                            );
                        }
                        match pi_memory_core::runtime::build_recall_injection(
                            storage,
                            &params.prompt,
                            &params.project_scope,
                            &params.session_id,
                            shared,
                            vectors,
                        ) {
                            Ok(text) => RpcResponse::ok(request.id, serde_json::json!(text)),
                            Err(e) => {
                                RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string())
                            }
                        }
                    }
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid buildRecallInjection params",
                ),
            }
        }
        methods::ENQUEUE_TURN => {
            match serde_json::from_value::<EnqueueTurnParams>(request.params.clone()) {
                Ok(params) if params.text.len() <= 65_536 => {
                    match pool.get_or_open(&params.project_scope) {
                        Ok(runtime) => {
                            let runtime = lock_project(&runtime);
                            match runtime.storage.enqueue_pending_turn(
                                &params.session_id,
                                params.turn_index,
                                &params.project_scope,
                                &params.text,
                                params.prompt_missing,
                            ) {
                                Ok(inserted) => RpcResponse::ok(
                                    request.id,
                                    serde_json::json!({ "inserted": inserted }),
                                ),
                                Err(error) => RpcResponse::err(
                                    request.id,
                                    codes::INTERNAL_ERROR,
                                    error.to_string(),
                                ),
                            }
                        }
                        Err(error) => {
                            RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                        }
                    }
                }
                Ok(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "turn text exceeds 64 KiB",
                ),
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid enqueueTurn params",
                ),
            }
        }
        methods::CLAIM_PENDING_TURN => {
            match serde_json::from_value::<ClaimPendingTurnParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime.storage.claim_pending_turn(
                            &params.session_id,
                            &params.project_scope,
                            &params.owner,
                            params.lease_ms.unwrap_or(300_000).clamp(5_000, 900_000),
                        ) {
                            Ok(turn) => RpcResponse::ok(
                                request.id,
                                serde_json::to_value(turn).unwrap_or(serde_json::Value::Null),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid claimPendingTurn params",
                ),
            }
        }
        methods::HEARTBEAT_PENDING_TURN => {
            match serde_json::from_value::<PendingTurnLeaseParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime.storage.heartbeat_pending_turn(
                            &params.session_id,
                            params.turn_index,
                            &params.owner,
                            params.lease_ms.unwrap_or(300_000).clamp(5_000, 900_000),
                        ) {
                            Ok(renewed) => RpcResponse::ok(
                                request.id,
                                serde_json::json!({ "renewed": renewed }),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid heartbeatPendingTurn params",
                ),
            }
        }
        methods::COMPLETE_PENDING_TURN => {
            match serde_json::from_value::<PendingTurnOwnerParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime.storage.complete_pending_turn(
                            &params.session_id,
                            params.turn_index,
                            &params.owner,
                        ) {
                            Ok(completed) => RpcResponse::ok(
                                request.id,
                                serde_json::json!({ "completed": completed }),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid completePendingTurn params",
                ),
            }
        }
        methods::FAIL_PENDING_TURN => {
            match serde_json::from_value::<FailPendingTurnParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime.storage.fail_pending_turn(
                            &params.session_id,
                            params.turn_index,
                            &params.owner,
                            &params.error,
                        ) {
                            Ok(failed) => {
                                RpcResponse::ok(request.id, serde_json::json!({ "failed": failed }))
                            }
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid failPendingTurn params",
                ),
            }
        }
        methods::LIST_PENDING_TURNS => {
            match serde_json::from_value::<ListPendingTurnsParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime
                            .storage
                            .list_pending_turns(&params.session_id, params.limit.unwrap_or(100))
                        {
                            Ok(turns) => RpcResponse::ok(request.id, serde_json::json!(turns)),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid listPendingTurns params",
                ),
            }
        }
        methods::RECORD_COMPACTION => {
            match serde_json::from_value::<RecordCompactionParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        let now = pi_memory_core::runtime::now_iso();
                        let event_id = pi_memory_core::ids::stable_id(
                            &params.session_id,
                            "compaction",
                            &[
                                &params.reason,
                                &params.summary_hash,
                                &params.first_kept_entry_id,
                            ],
                        );
                        let description = format!(
                            "Session compaction: reason={}, summary_sha256={}, first_kept_entry_id={}",
                            params.reason, params.summary_hash, params.first_kept_entry_id,
                        );
                        let transaction = (|| -> anyhow::Result<()> {
                            let state = runtime.storage.get_session_state(&params.session_id)?;
                            runtime.storage.begin_tx()?;
                            runtime.storage.insert_timeline_events(&[
                                pi_memory_core::models::TimelineEventRecord {
                                    event_id,
                                    date: now.clone(),
                                    description,
                                    source: Some("compaction".into()),
                                    source_memory_id: None,
                                    session_id: params.session_id.clone(),
                                    project_scope: params.project_scope.clone(),
                                    created_at: now,
                                },
                            ])?;
                            runtime.storage.save_session_state(
                                &pi_memory_core::models::SessionStateRecord {
                                    session_id: params.session_id,
                                    project_scope: params.project_scope,
                                    last_retained_user_turn: state
                                        .map(|value| value.last_retained_user_turn)
                                        .unwrap_or(0),
                                    has_recalled_for_first_turn: false,
                                },
                            )?;
                            runtime.storage.commit_tx()?;
                            Ok(())
                        })();
                        match transaction {
                            Ok(()) => {
                                RpcResponse::ok(request.id, serde_json::json!({ "recorded": true }))
                            }
                            Err(error) => {
                                let _ = runtime.storage.rollback_tx();
                                RpcResponse::err(
                                    request.id,
                                    codes::INTERNAL_ERROR,
                                    error.to_string(),
                                )
                            }
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid recordCompaction params",
                ),
            }
        }
        methods::BACKFILL_EMBEDDINGS => {
            match serde_json::from_value::<BackfillEmbeddingsParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let mut runtime = lock_project(&runtime);
                        let ProjectRuntime { storage, vectors } = &mut *runtime;
                        match pi_memory_core::runtime::backfill_embeddings(
                            storage,
                            shared,
                            vectors,
                            params.max_items.unwrap_or(32).clamp(1, 128),
                        ) {
                            Ok(result) => RpcResponse::ok(
                                request.id,
                                serde_json::to_value(result).expect("serialize result"),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid backfillEmbeddings params",
                ),
            }
        }
        methods::RETAIN_TURN_MEMORIES => {
            match serde_json::from_value::<RetainParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let mut runtime = lock_project(&runtime);
                        let ProjectRuntime { storage, vectors } = &mut *runtime;
                        match pi_memory_core::runtime::retain_turn_memories_with_prompt_state(
                            storage,
                            params.turn_index,
                            &params.text,
                            &params.session_id,
                            &params.project_scope,
                            shared,
                            vectors,
                            params.llm_result.as_deref(),
                            params.prompt_missing,
                        ) {
                            Ok(()) => RpcResponse::ok(request.id, serde_json::json!(null)),
                            Err(e) => {
                                RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string())
                            }
                        }
                    }
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid retainTurnMemories params",
                ),
            }
        }
        methods::GET_WORKING_CONTENT => {
            match serde_json::from_value::<GetWorkingContentParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        let id = format!("{}:turn:{}", params.session_id, params.turn_index);
                        match runtime.storage.get_working(&id) {
                            Ok(Some(memory))
                                if memory.project_scope == params.project_scope
                                    && memory.session_id == params.session_id =>
                            {
                                RpcResponse::ok(
                                    request.id,
                                    serde_json::json!({"text": memory.content}),
                                )
                            }
                            Ok(_) => RpcResponse::ok(request.id, serde_json::Value::Null),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid getWorkingContent params",
                ),
            }
        }
        methods::CONSOLIDATE_SESSION_MEMORIES => {
            if std::env::var("PI_MEMORY_LEGACY_CONSOLIDATION")
                .ok()
                .as_deref()
                != Some("1")
            {
                RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "SNAPSHOT_REQUIRED: use claim/commit consolidation RPCs",
                )
            } else {
                match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                    Ok(params) => match pool.get_or_open(&params.project_scope) {
                        Ok(runtime) => {
                            let runtime = lock_project(&runtime);
                            match pi_memory_core::runtime::consolidate_session_memories(
                                &runtime.storage,
                                &params.session_id,
                                &params.project_scope,
                                params.llm_result.as_deref(),
                                params.llm_failed,
                            ) {
                                Ok(result) => RpcResponse::ok(
                                    request.id,
                                    serde_json::to_value(result)
                                        .expect("serialization cannot fail"),
                                ),
                                Err(e) => RpcResponse::err(
                                    request.id,
                                    codes::INTERNAL_ERROR,
                                    e.to_string(),
                                ),
                            }
                        }
                        Err(e) => {
                            RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string())
                        }
                    },
                    Err(_) => RpcResponse::err(
                        request.id,
                        codes::INVALID_PARAMS,
                        "invalid consolidateSessionMemories params",
                    ),
                }
            }
        }
        methods::CLAIM_CONSOLIDATION => {
            match serde_json::from_value::<ClaimConsolidationParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        let claim_token = pool.new_consolidation_claim_token();
                        match pi_memory_core::runtime::claim_consolidation(
                            &runtime.storage,
                            &params.session_id,
                            &params.project_scope,
                            &claim_token,
                            params.ttl_ms.unwrap_or(180_000),
                            params.limit.unwrap_or(100),
                        ) {
                            Ok(result) => RpcResponse::ok(
                                request.id,
                                serde_json::to_value(result).expect("serialization cannot fail"),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid claimConsolidation params",
                ),
            }
        }
        methods::COUNT_UNCONSOLIDATED => {
            match serde_json::from_value::<CountUnconsolidatedParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match runtime
                            .storage
                            .count_unconsolidated(&params.session_id, &params.project_scope)
                        {
                            Ok(count) => match runtime.storage.is_consolidation_dirty() {
                                Ok(dirty) => {
                                    let dirty_result = if count == 0 {
                                        runtime
                                            .storage
                                            .clear_consolidation_dirty_if_idle(
                                                &params.project_scope,
                                            )
                                            .map(|()| false)
                                    } else {
                                        Ok(dirty)
                                    };
                                    match dirty_result {
                                        Ok(dirty) => RpcResponse::ok(
                                            request.id,
                                            serde_json::json!({ "count": count, "dirty": dirty }),
                                        ),
                                        Err(error) => RpcResponse::err(
                                            request.id,
                                            codes::INTERNAL_ERROR,
                                            error.to_string(),
                                        ),
                                    }
                                }
                                Err(error) => RpcResponse::err(
                                    request.id,
                                    codes::INTERNAL_ERROR,
                                    error.to_string(),
                                ),
                            },
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid countUnconsolidated params",
                ),
            }
        }
        methods::HEARTBEAT_CONSOLIDATION => {
            match serde_json::from_value::<LeaseParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match pi_memory_core::runtime::heartbeat_consolidation(
                            &runtime.storage,
                            &params.claim_token,
                            params.ttl_ms.unwrap_or(180_000),
                        ) {
                            Ok(renewed) => RpcResponse::ok(
                                request.id,
                                serde_json::json!({ "renewed": renewed }),
                            ),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid heartbeatConsolidation params",
                ),
            }
        }
        methods::RELEASE_CONSOLIDATION => {
            match serde_json::from_value::<LeaseParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        match pi_memory_core::runtime::release_consolidation(
                            &runtime.storage,
                            &params.claim_token,
                        ) {
                            Ok(()) => {
                                RpcResponse::ok(request.id, serde_json::json!({ "released": true }))
                            }
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid releaseConsolidation params",
                ),
            }
        }
        methods::COMMIT_CONSOLIDATION => {
            match serde_json::from_value::<CommitConsolidationParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let mut runtime = lock_project(&runtime);
                        let ProjectRuntime { storage, vectors } = &mut *runtime;
                        match pi_memory_core::runtime::commit_consolidation(
                            storage,
                            &params.claim_token,
                            params.summary.as_deref(),
                            params.degraded,
                            shared,
                            vectors,
                        ) {
                            Ok(result) => RpcResponse::ok(request.id, result),
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    }
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INTERNAL_ERROR, error.to_string())
                    }
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid commitConsolidation params",
                ),
            }
        }
        methods::CONSOLIDATE_SOURCE => {
            match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let runtime = lock_project(&runtime);
                        let text = pi_memory_core::runtime::consolidate_source_text(
                            &runtime.storage,
                            &params.session_id,
                            &params.project_scope,
                        );
                        RpcResponse::ok(request.id, serde_json::json!(text))
                    }
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid consolidateSource params",
                ),
            }
        }
        methods::MEMORY_COMMAND => {
            match serde_json::from_value::<MemoryCommandParams>(request.params.clone()) {
                Ok(params) => {
                    if params.sub == "clear" && params.target.trim() == "all" {
                        RpcResponse::err(
                            request.id,
                            codes::INVALID_PARAMS,
                            "clear all is destructive; repeat as `/memory clear all confirm`",
                        )
                    } else if params.sub == "clear" && params.target.trim() == "all confirm" {
                        match pool.clear_all_memories() {
                            Ok(report) => {
                                RpcResponse::ok(request.id, serde_json::json!(report.to_string()))
                            }
                            Err(error) => RpcResponse::err(
                                request.id,
                                codes::INTERNAL_ERROR,
                                error.to_string(),
                            ),
                        }
                    } else {
                        // A targeted clear must address the target project's physical database,
                        // rather than deleting matching rows in the caller's database.
                        let effective_scope = if params.sub == "clear" && !params.target.is_empty()
                        {
                            params.target.as_str()
                        } else {
                            params.project_scope.as_str()
                        };
                        match pool.get_or_open(effective_scope) {
                            Ok(runtime) => {
                                let mut runtime = lock_project(&runtime);
                                if params.sub == "purge" {
                                    match runtime.storage.purge_expired(
                                        effective_scope,
                                        &pi_memory_core::runtime::now_iso(),
                                    ) {
                                        Ok(removed) => {
                                            let count = removed;
                                            match runtime.storage.purge_orphan_embeddings() {
                                                Ok(orphan_embeddings) => {
                                                    let ProjectRuntime { storage, vectors } =
                                                        &mut *runtime;
                                                    let index_result = rehydrate_project_vectors(
                                                        storage, vectors, false,
                                                    );
                                                    match index_result {
                                                        Ok(()) => {
                                                            let fts_dirty = storage
                                                                .fts_rebuild_needed()
                                                                .unwrap_or(true);
                                                            if fts_dirty {
                                                                RpcResponse::ok(
                                                                    request.id,
                                                                    serde_json::json!(format!(
                                                                        "purge: removed={count} orphan_embeddings={orphan_embeddings} index_state=partial fts_dirty=true needs_rehydrate=false"
                                                                    )),
                                                                )
                                                            } else {
                                                                RpcResponse::ok(
                                                                    request.id,
                                                                    serde_json::json!(format!(
                                                                        "purge: removed={count} orphan_embeddings={orphan_embeddings} index_state=ok"
                                                                    )),
                                                                )
                                                            }
                                                        }
                                                        Err(error) => RpcResponse::ok(
                                                            request.id,
                                                            serde_json::json!(format!(
                                                                "purge: db_committed=true removed={count} orphan_embeddings={orphan_embeddings} index_state=partial needs_rehydrate=true error={error}"
                                                            )),
                                                        ),
                                                    }
                                                }
                                                Err(error) => {
                                                    let ProjectRuntime { storage, vectors } =
                                                        &mut *runtime;
                                                    let _ = storage.mark_vector_rehydrate_failed(
                                                        &error.to_string(),
                                                    );
                                                    if let Ok(empty) = ProjectVectors::new() {
                                                        *vectors = empty;
                                                    }
                                                    RpcResponse::ok(
                                                        request.id,
                                                        serde_json::json!(format!(
                                                            "purge: db_committed=true removed={count} index_state=partial needs_rehydrate=true error={error}"
                                                        )),
                                                    )
                                                }
                                            }
                                        }
                                        Err(error) => RpcResponse::err(
                                            request.id,
                                            codes::INTERNAL_ERROR,
                                            error.to_string(),
                                        ),
                                    }
                                } else {
                                    let result = pi_memory_core::runtime::memory_command(
                                        &runtime.storage,
                                        &params.sub,
                                        "",
                                        effective_scope,
                                        &params.session_id,
                                        &shared.status(),
                                    );
                                    match result {
                                        Ok(text) => {
                                            if params.sub == "clear" {
                                                let ProjectRuntime { storage, vectors } =
                                                    &mut *runtime;
                                                match rebuild_project_indexes(storage, vectors) {
                                                    Ok(()) => RpcResponse::ok(
                                                        request.id,
                                                        serde_json::json!(format!(
                                                            "{text}; db_committed=true index_state=ok"
                                                        )),
                                                    ),
                                                    Err(error) => RpcResponse::ok(
                                                        request.id,
                                                        serde_json::json!(format!(
                                                            "{text}; db_committed=true index_state=partial needs_rehydrate=true error={error}"
                                                        )),
                                                    ),
                                                }
                                            } else if params.sub == "diagnose" {
                                                let db = runtime.storage.diagnose().unwrap_or_else(
                                                    |error| serde_json::json!({ "error": error.to_string() }),
                                                );
                                                let recall = pi_memory_core::runtime::last_recall_diagnostics(effective_scope);
                                                RpcResponse::ok(
                                                    request.id,
                                                    serde_json::json!(format!(
                                                        "{text}; storage_diagnostics={db}; last_recall={}",
                                                        serde_json::to_string(&recall)
                                                            .unwrap_or_else(|_| "{}".into())
                                                    )),
                                                )
                                            } else {
                                                RpcResponse::ok(request.id, serde_json::json!(text))
                                            }
                                        }
                                        Err(e) => RpcResponse::err(
                                            request.id,
                                            codes::INTERNAL_ERROR,
                                            e.to_string(),
                                        ),
                                    }
                                }
                            }
                            Err(e) => {
                                RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string())
                            }
                        }
                    }
                }
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid memoryCommand params",
                ),
            }
        }
        methods::MIGRATE => match serde_json::from_value::<MigrateParams>(request.params.clone()) {
            Ok(params) => match pool.get_or_open(&params.project_scope) {
                Ok(runtime) => {
                    let mut runtime = lock_project(&runtime);
                    let ProjectRuntime { storage, vectors } = &mut *runtime;
                    match storage
                        .migrate_from_scope(&params.source_path, Some(&params.project_scope))
                    {
                        Ok(stats) => {
                            let fts_dirty = stats
                                .get("fts_dirty")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false);
                            let index_result =
                                rehydrate_project_vectors(storage, vectors, !fts_dirty)
                                    .map(|()| !fts_dirty);
                            match index_result {
                                Ok(true) => RpcResponse::ok(
                                    request.id,
                                    serde_json::json!({
                                        "db_committed": true,
                                        "index_state": "ok",
                                        "target_scope": params.project_scope,
                                        "imported": stats,
                                    }),
                                ),
                                Ok(false) => RpcResponse::ok(
                                    request.id,
                                    serde_json::json!({
                                        "db_committed": true,
                                        "index_state": "partial",
                                        "fts_dirty": true,
                                        "needs_rehydrate": false,
                                        "target_scope": params.project_scope,
                                        "imported": stats,
                                    }),
                                ),
                                Err(error) => RpcResponse::ok(
                                    request.id,
                                    serde_json::json!({
                                        "db_committed": true,
                                        "index_state": "partial",
                                        "needs_rehydrate": true,
                                        "target_scope": params.project_scope,
                                        "imported": stats,
                                        "error": error.to_string(),
                                    }),
                                ),
                            }
                        }
                        Err(e) => {
                            RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string())
                        }
                    }
                }
                Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
            },
            Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid migrate params"),
        },
        methods::PREPARE_MODEL => {
            start_model_warmup(shared, pool);
            RpcResponse::ok(request.id, serde_json::json!("preparing"))
        }
        methods::START_JOB => {
            match serde_json::from_value::<StartJobParams>(request.params.clone()) {
                Ok(params) => match pool.start_job(params, shared.clone()) {
                    Ok(job) => RpcResponse::ok(
                        request.id,
                        serde_json::to_value(job).expect("job status serializes"),
                    ),
                    Err(error) => {
                        RpcResponse::err(request.id, codes::INVALID_PARAMS, error.to_string())
                    }
                },
                Err(_) => {
                    RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid startJob params")
                }
            }
        }
        methods::JOB_STATUS => {
            match serde_json::from_value::<JobStatusParams>(request.params.clone()) {
                Ok(params) => match pool.job_status(&params.job_id) {
                    Some(job) => RpcResponse::ok(
                        request.id,
                        serde_json::to_value(job).expect("job status serializes"),
                    ),
                    None => RpcResponse::err(request.id, codes::INVALID_PARAMS, "unknown jobId"),
                },
                Err(_) => RpcResponse::err(
                    request.id,
                    codes::INVALID_PARAMS,
                    "invalid jobStatus params",
                ),
            }
        }
        methods::LLM_PROMPTS => RpcResponse::ok(
            request.id,
            serde_json::json!({
                "extract": pi_memory_core::llm::EXTRACT_PROMPT,
                "consolidate": pi_memory_core::llm::CONSOLIDATE_PROMPT,
            }),
        ),
        methods::SHUTDOWN => {
            // The main loop exits after responding
            RpcResponse::ok(request.id, serde_json::json!({ "bye": true }))
        }
        _ => RpcResponse::err(request.id, codes::METHOD_NOT_FOUND, "unknown method"),
    };
    serde_json::to_value(response).expect("serialization cannot fail")
}

// ---------------------------------------------------------------------------
// RPC param types
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RecallParams {
    #[serde(default)]
    prompt: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueTurnParams {
    session_id: String,
    turn_index: i64,
    project_scope: String,
    text: String,
    #[serde(default)]
    prompt_missing: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimPendingTurnParams {
    session_id: String,
    project_scope: String,
    owner: String,
    lease_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingTurnLeaseParams {
    session_id: String,
    project_scope: String,
    turn_index: i64,
    owner: String,
    lease_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingTurnOwnerParams {
    session_id: String,
    project_scope: String,
    turn_index: i64,
    owner: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailPendingTurnParams {
    session_id: String,
    project_scope: String,
    turn_index: i64,
    owner: String,
    error: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPendingTurnsParams {
    session_id: String,
    project_scope: String,
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordCompactionParams {
    session_id: String,
    project_scope: String,
    reason: String,
    summary_hash: String,
    first_kept_entry_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackfillEmbeddingsParams {
    project_scope: String,
    max_items: Option<usize>,
}

#[derive(serde::Deserialize)]
struct RetainParams {
    #[serde(rename = "turnIndex")]
    turn_index: i64,
    text: String,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "llmResult", default)]
    llm_result: Option<String>,
    #[serde(rename = "promptMissing", default)]
    prompt_missing: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetWorkingContentParams {
    session_id: String,
    turn_index: i64,
    project_scope: String,
}

#[derive(serde::Deserialize)]
struct ConsolidateParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "llmResult", default)]
    llm_result: Option<String>,
    #[serde(rename = "llmFailed", default)]
    llm_failed: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimConsolidationParams {
    session_id: String,
    project_scope: String,
    ttl_ms: Option<u64>,
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountUnconsolidatedParams {
    session_id: String,
    project_scope: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseParams {
    project_scope: String,
    claim_token: String,
    ttl_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitConsolidationParams {
    project_scope: String,
    claim_token: String,
    summary: Option<String>,
    #[serde(default)]
    degraded: bool,
}

#[derive(serde::Deserialize)]
struct MigrateParams {
    #[serde(rename = "sourcePath")]
    source_path: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobStatusParams {
    job_id: String,
}

#[derive(serde::Deserialize)]
struct MemoryCommandParams {
    sub: String,
    #[serde(default)]
    target: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

// ---------------------------------------------------------------------------
// Frame protocol
// ---------------------------------------------------------------------------

/// Read a single frame: `<u32 big-endian length><JSON>`
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Write a frame
fn write_frame(writer: &mut impl Write, payload: &str) -> std::io::Result<()> {
    let bytes = payload.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too long"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

// Keep Arc referenced for type consistency (SharedEmbedder already uses Arc internally)
#[allow(dead_code)]
fn _keep_arc(_: &Arc<SharedEmbedder>) {}

#[cfg(test)]
mod tests {
    use super::{
        evict_idle_project_runtimes, lock_project, read_frame, write_frame, ConnectionCountGuard,
        ProjectEntry, ProjectRuntime, StoragePool,
    };
    use pi_memory_core::embedding_runtime::ProjectVectors;
    use pi_memory_core::storage::TursoStorage;
    use std::io::Cursor;
    use std::sync::{mpsc, Arc, Mutex};

    /// 确定性伪随机：同一种子生成同样的字节序列，保证可复现。
    fn seeded_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn read_frame_tolerates_malformed_input() {
        // 各种畸形输入必须返回 Ok/Err，绝不 panic
        // 1) 空输入 → Ok(None)
        assert!(read_frame(&mut &b""[..]).unwrap().is_none());
        // 2) 只读 1-3 字节长度前缀（截断）→ Ok(None)
        assert!(read_frame(&mut &b"\x00"[..]).unwrap().is_none());
        assert!(read_frame(&mut &b"\x00\x00"[..]).unwrap().is_none());
        assert!(read_frame(&mut &b"\x00\x00\x00"[..]).unwrap().is_none());
        // 3) 长度前缀完整但帧体截断 → Err(UnexpectedEof)（与长度前缀截断的 Ok(None) 语义区分；
        //    两者都会让 serve 循环退出，Err 标记异常流）
        assert!(read_frame(&mut &b"\x00\x00\x00\x05ab"[..]).is_err());
        // 4) 超大帧（>16MB）→ Err
        assert!(read_frame(&mut &b"\xff\xff\xff\xff"[..]).is_err());
        assert!(read_frame(&mut &b"\x01\x00\x00\x00"[..]).is_err());
        // 5) 零长度帧 → Ok(Some(""))
        assert_eq!(
            read_frame(&mut &b"\x00\x00\x00\x00"[..]).unwrap(),
            Some(String::new())
        );
        // 6) 非 UTF-8 帧体 → 不 panic（lossy 替换）
        let raw = [0u8, 0, 0, 1, 0xFF];
        assert!(read_frame(&mut &raw[..]).is_ok());
        // 7) 确定性 fuzz：1000 组随机字节不 panic
        for seed in 0..1000u64 {
            let data = seeded_bytes(seed, (seed % 64) as usize);
            let _ = read_frame(&mut &data[..]);
        }
        // 8) 畸形负载 + 随机长度前缀组合
        for seed in 0..500u64 {
            let data = seeded_bytes(seed, 8);
            let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let _ = read_frame(&mut &data[..]);
            let _ = std::hint::black_box(len);
        }
    }

    #[test]
    fn frame_roundtrip_and_max_length() {
        // 正常往返
        let mut buf = Vec::new();
        write_frame(&mut buf, r#"{"id":1,"method":"hello"}"#).unwrap();
        let parsed = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(parsed, r#"{"id":1,"method":"hello"}"#);
        // 超大 payload 写入报错（>u32）
        let huge = "x".repeat(u32::MAX as usize + 8);
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &huge).is_err());
        // 混合帧流：多帧连续解析
        let mut stream = Vec::new();
        for i in 0..10 {
            let payload = format!(r#"{{"id":{i}}}"#);
            write_frame(&mut stream, &payload).unwrap();
        }
        let mut cursor = Cursor::new(stream);
        for i in 0..10 {
            let frame = read_frame(&mut cursor).unwrap().unwrap();
            assert_eq!(frame, format!(r#"{{"id":{i}}}"#));
        }
    }

    #[test]
    fn recall_params_default_missing_prompt() {
        // custom 消息触发的运行没有 prompt，缺字段不能退化为 INVALID_PARAMS
        let params: super::RecallParams = serde_json::from_value(serde_json::json!({
            "projectScope": "/tmp/p",
            "sessionId": "s1",
        }))
        .expect("missing prompt must default");
        assert!(params.prompt.is_empty());
    }

    #[test]
    fn recall_params_keep_prompt_when_present() {
        let params: super::RecallParams = serde_json::from_value(serde_json::json!({
            "prompt": "hello",
            "projectScope": "/tmp/p",
            "sessionId": "s1",
        }))
        .expect("valid params must deserialize");
        assert_eq!(params.prompt, "hello");
    }

    #[test]
    fn busy_project_runtime_does_not_block_another_project() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pi-memory-server-project-lock-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temporary data directory");
        let pool = StoragePool::new();
        let runtime_for = |scope: &str| {
            let path = data_dir.join(format!("{}.db", scope.replace('/', "_")));
            let storage =
                TursoStorage::open_deferred(path.to_str().unwrap()).expect("open deferred storage");
            let vectors = ProjectVectors::new().expect("create project vectors");
            Arc::new(Mutex::new(ProjectRuntime { storage, vectors }))
        };
        let project_a = runtime_for("/project/a");
        let project_b = runtime_for("/project/b");
        {
            let mut projects = pool.lock_projects();
            projects.insert(
                "/project/a".into(),
                ProjectEntry {
                    runtime: Arc::clone(&project_a),
                    last_access: 1,
                },
            );
            projects.insert(
                "/project/b".into(),
                ProjectEntry {
                    runtime: Arc::clone(&project_b),
                    last_access: 2,
                },
            );
        }

        let busy_guard = lock_project(&project_a);
        let worker_pool = pool.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_pool.get_or_open("/project/b").map(|runtime| {
                let runtime = lock_project(&runtime);
                runtime.storage.db_path().to_string()
            });
            let _ = sender.send(result);
        });
        let other_project_progress = receiver.recv_timeout(std::time::Duration::from_millis(500));
        drop(busy_guard);
        worker
            .join()
            .expect("other project worker should not panic");
        assert!(
            other_project_progress.is_ok(),
            "project B should enter its runtime while project A is busy"
        );

        drop(project_a);
        drop(project_b);
        drop(pool);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn project_runtime_lru_skips_shared_runtimes_and_evicts_oldest_idle() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pi-memory-server-runtime-lru-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temporary data directory");
        let runtime_for = |name: &str| {
            let path = data_dir.join(format!("{name}.db"));
            let storage =
                TursoStorage::open_deferred(path.to_str().unwrap()).expect("open test database");
            Arc::new(Mutex::new(ProjectRuntime {
                storage,
                vectors: ProjectVectors::new().expect("create vector index"),
            }))
        };
        let a = runtime_for("a");
        let b = runtime_for("b");
        let c = runtime_for("c");
        let protected_a = Arc::clone(&a);
        let mut projects = std::collections::HashMap::new();
        projects.insert(
            "a".into(),
            ProjectEntry {
                runtime: a,
                last_access: 1,
            },
        );
        projects.insert(
            "b".into(),
            ProjectEntry {
                runtime: b,
                last_access: 2,
            },
        );
        projects.insert(
            "c".into(),
            ProjectEntry {
                runtime: c,
                last_access: 3,
            },
        );

        assert_eq!(evict_idle_project_runtimes(&mut projects, "c", 2), 1);
        assert!(projects.contains_key("a"), "in-use runtime must stay open");
        assert!(
            !projects.contains_key("b"),
            "oldest idle runtime is evicted"
        );
        assert!(projects.contains_key("c"), "current runtime must stay open");

        drop(protected_a);
        assert_eq!(evict_idle_project_runtimes(&mut projects, "c", 1), 1);
        assert!(
            !projects.contains_key("a"),
            "newly idle runtime can be evicted"
        );
        assert!(projects.contains_key("c"));

        drop(projects);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn connection_count_guard_decrements_during_panic_unwind() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let guard_count = Arc::clone(&count);
        let panic = std::panic::catch_unwind(move || {
            let _guard = ConnectionCountGuard(guard_count);
            panic!("simulated socket handler panic");
        });
        assert!(panic.is_err());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn poisoned_project_runtime_recovers_and_rehydrates() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!(
            "pi-memory-server-poison-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temporary data directory");
        let storage =
            TursoStorage::open_deferred(data_dir.join("project.db").to_str().expect("UTF-8 path"))
                .expect("open deferred storage");
        let runtime = Arc::new(Mutex::new(ProjectRuntime {
            storage,
            vectors: ProjectVectors::new().expect("create vector index"),
        }));

        let poisoned_runtime = Arc::clone(&runtime);
        let panic = std::panic::catch_unwind(move || {
            let _guard = poisoned_runtime.lock().expect("initial runtime lock");
            panic!("simulate a panic while the project runtime is locked");
        });
        assert!(panic.is_err());
        assert!(runtime.is_poisoned());

        let recovered = lock_project(&runtime);
        assert_eq!(recovered.vectors.index_len(), 0);
        assert!(!runtime.is_poisoned());
        drop(recovered);
        drop(runtime);
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
