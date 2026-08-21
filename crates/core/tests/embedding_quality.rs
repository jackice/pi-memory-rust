//! 语义 embedding 质量验证（需要真实模型，默认忽略）。
//! 运行：模型就绪后 `cargo test -p pi-memory-core --test embedding_quality -- --ignored`

use pi_memory_core::embedding::Embedder;

#[test]
#[ignore = "需要真实模型文件"]
fn semantic_similarity_quality() {
    let model_dir = std::env::var("PI_MEMORY_MODEL_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("pi-memory-model"));
    let mut embedder = Embedder::load(&model_dir).expect("模型加载失败");
    assert_eq!(embedder.dim(), 384);

    let similar_a = "用户偏好使用中文回复".to_string();
    let similar_b = "用户喜欢用中文交流".to_string();
    let different = "项目使用 turbovec 向量索引".to_string();

    let vectors = embedder.embed(&[similar_a.clone(), similar_b, different]).expect("embed 失败");
    let (a, b, c) = (&vectors[0], &vectors[1], &vectors[2]);

    let sim_ab: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let sim_ac: f32 = a.iter().zip(c.iter()).map(|(x, y)| x * y).sum();

    println!("相似文本余弦: {sim_ab:.4}，不同文本余弦: {sim_ac:.4}");
    assert!(sim_ab > sim_ac, "相似文本相似度应显著高于不同文本（{sim_ab} vs {sim_ac}）");
    assert!(sim_ab > 0.5, "相似文本余弦应较高（实际 {sim_ab}）");
}
