mod common;

use common::TEST_MODEL;

#[test]
fn test_attention_mask_with_padding() {
    // Encode a short text that will be padded
    let embedding = TEST_MODEL.encode_single("cat").expect("Failed to encode");

    // Verify embedding is 384-dimensional and normalized
    assert_eq!(embedding.len(), 384, "Embedding should be 384-dimensional");

    let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 0.01,
        "Embedding should be normalized (L2 norm ≈ 1.0), got {:.4}",
        norm
    );
}

#[test]
fn test_regression_padding_bug() {
    // This test ensures the bug we fixed (using all-1s attention mask) doesn't regress
    let emb1 = TEST_MODEL.encode_single("cat").expect("Failed to encode cat");
    let emb2 = TEST_MODEL.encode_single("dog").expect("Failed to encode dog");

    // Compute cosine similarity
    let similarity = emb1.iter().zip(&emb2)
        .map(|(a, b)| a * b)
        .sum::<f32>();

    // With the bug, this was ~0.97. Fixed, it should be ~0.76
    assert!(
        similarity < 0.90,
        "Similarity between 'cat' and 'dog' should be < 0.90 (bug would give ~0.97), got {:.4}",
        similarity
    );

    assert!(
        similarity > 0.60,
        "Similarity should still be > 0.60 for related words, got {:.4}",
        similarity
    );
}

#[test]
fn test_batch_encoding() {
    let texts = vec![
        "cat".to_string(),
        "dog".to_string(),
        "The weather is lovely".to_string(),
    ];

    let embeddings = TEST_MODEL.encode_batch(&texts).expect("Failed to encode batch");

    assert_eq!(embeddings.len(), 3, "Should have 3 embeddings");

    for (i, embedding) in embeddings.iter().enumerate() {
        assert_eq!(
            embedding.len(),
            384,
            "Embedding {} should be 384-dimensional",
            i
        );

        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "Embedding {} should be normalized, got norm {:.4}",
            i,
            norm
        );
    }
}
