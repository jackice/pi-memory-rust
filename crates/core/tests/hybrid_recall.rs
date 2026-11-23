//! Integration coverage for independent FTS and vector candidate retrieval.

use pi_memory_core::embedding_runtime::{
    active_model_id, EmbeddingProvider, FakeEmbedder, ProjectVectors,
};
use pi_memory_core::models::{MemoryEmbeddingRecord, MemoryKind, WorkingMemoryRecord};
use pi_memory_core::runtime::{build_recall_injection, collect_recall_sources};
use pi_memory_core::storage::TursoStorage;

fn setup_db(label: &str) -> (std::path::PathBuf, TursoStorage) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "pi-memory-hybrid-recall-{}-{label}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memory.db");
    let storage = TursoStorage::open(path.to_str().unwrap()).unwrap();
    (dir, storage)
}

fn working(id: &str, content: &str) -> WorkingMemoryRecord {
    WorkingMemoryRecord {
        id: id.into(),
        content: content.into(),
        embed_text: None,
        source: Some("test".into()),
        session_id: "session".into(),
        project_scope: "/project/hybrid".into(),
        importance: 0.5,
        memory_type: "working".into(),
        metadata: None,
        created_at: "2026-11-22T00:00:00Z".into(),
        updated_at: "2026-11-22T00:00:00Z".into(),
        consolidated_at: None,
        valid_until: None,
        superseded_by: None,
    }
}

fn save_vector(storage: &TursoStorage, id: &str, embedding: Vec<f32>) {
    storage
        .save_embedding(&MemoryEmbeddingRecord {
            memory_kind: MemoryKind::Working,
            memory_id: id.into(),
            embedding: embedding.into_iter().map(f64::from).collect(),
            model: active_model_id(),
        })
        .unwrap();
}

fn cleanup(dir: std::path::PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn vector_hit_outside_fts_is_included_in_hybrid_recall() {
    let (dir, storage) = setup_db("vector-outside-fts");
    storage
        .insert_working(&working("fts-distractor", "needle lexical distractor"))
        .unwrap();
    storage
        .insert_working(&working(
            "vector-target",
            "semantically relevant memory without the query term",
        ))
        .unwrap();

    let query = "needle";
    let fake = FakeEmbedder;
    let query_vector = fake
        .embed_query(&[query.to_string()])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    save_vector(&storage, "fts-distractor", vec![0.0; query_vector.len()]);
    save_vector(&storage, "vector-target", query_vector);
    let mut vectors = ProjectVectors::new().unwrap();
    vectors.hydrate(&storage).unwrap();

    let sources =
        collect_recall_sources(&storage, query, "/project/hybrid", &fake, &mut vectors).unwrap();
    assert_eq!(
        sources
            .fts
            .iter()
            .map(|item| item.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec!["fts-distractor"]
    );
    assert_eq!(
        sources
            .vector
            .iter()
            .map(|item| item.memory_id.as_str())
            .collect::<Vec<_>>(),
        vec!["vector-target"]
    );
    let injection = build_recall_injection(
        &storage,
        query,
        "/project/hybrid",
        "session",
        &fake,
        &mut vectors,
    )
    .unwrap();
    assert!(
        injection.contains("semantically relevant memory without the query term"),
        "global vector Top-K must survive an unrelated FTS candidate: {injection}"
    );

    drop(vectors);
    drop(storage);
    cleanup(dir);
}

#[test]
fn vector_top_k_is_nonempty_when_fts_returns_no_candidates() {
    let (dir, storage) = setup_db("vector-with-empty-fts");
    let query = "quuxxylophone";
    let fake = FakeEmbedder;
    let query_vector = fake
        .embed_query(&[query.to_string()])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    for index in 0..3 {
        let id = format!("vector-{index}");
        storage
            .insert_working(&working(&id, &format!("semantic memory number {index}")))
            .unwrap();
        save_vector(&storage, &id, query_vector.clone());
    }
    let mut vectors = ProjectVectors::new().unwrap();
    vectors.hydrate(&storage).unwrap();

    let sources =
        collect_recall_sources(&storage, query, "/project/hybrid", &fake, &mut vectors).unwrap();
    assert!(sources.fts.is_empty(), "fixture must have no lexical match");
    assert_eq!(
        sources.vector.len(),
        3,
        "vector search must request global Top-K even when FTS is empty"
    );

    drop(vectors);
    drop(storage);
    cleanup(dir);
}
