mod common;

use common::{cosine_similarity, TEST_MODEL};

#[test]
#[ignore] // Run with: cargo test debug_embeddings -- --ignored --nocapture
fn debug_embeddings() {
    // Compare a low similarity pair in detail
    let text1 = "The weather is lovely today";
    let text2 = "He drove to the stadium";

    println!("\n=== Debugging Low Similarity Pair ===");
    println!("Text 1: \"{}\"", text1);
    println!("Text 2: \"{}\"", text2);
    println!("Expected similarity: 0.1046");

    let emb1 = TEST_MODEL.encode_single(text1).expect("Failed to encode text1");
    let emb2 = TEST_MODEL.encode_single(text2).expect("Failed to encode text2");

    // Check normalization
    let norm1: f32 = emb1.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm2: f32 = emb2.iter().map(|x| x * x).sum::<f32>().sqrt();
    println!("\nNorms (should be ≈1.0):");
    println!("  Embedding 1: {:.6}", norm1);
    println!("  Embedding 2: {:.6}", norm2);

    // Check basic statistics
    let mean1 = emb1.iter().sum::<f32>() / emb1.len() as f32;
    let mean2 = emb2.iter().sum::<f32>() / emb2.len() as f32;
    println!("\nMeans:");
    println!("  Embedding 1: {:.6}", mean1);
    println!("  Embedding 2: {:.6}", mean2);

    // Check sparsity (how many near-zero values)
    let sparse1 = emb1.iter().filter(|&&x| x.abs() < 0.01).count();
    let sparse2 = emb2.iter().filter(|&&x| x.abs() < 0.01).count();
    println!("\nSparsity (values < 0.01):");
    println!("  Embedding 1: {}/{} ({:.1}%)", sparse1, emb1.len(), sparse1 as f32 / emb1.len() as f32 * 100.0);
    println!("  Embedding 2: {}/{} ({:.1}%)", sparse2, emb2.len(), sparse2 as f32 / emb2.len() as f32 * 100.0);

    // First 20 values
    println!("\nFirst 20 dimensions:");
    println!("  Emb1: {:?}", &emb1[..20].iter().map(|x| format!("{:.4}", x)).collect::<Vec<_>>());
    println!("  Emb2: {:?}", &emb2[..20].iter().map(|x| format!("{:.4}", x)).collect::<Vec<_>>());

    // Cosine similarity
    let similarity = cosine_similarity(&emb1, &emb2);
    println!("\nCosine Similarity: {:.4}", similarity);
    println!("Expected: 0.1046");
    println!("Error: {:.4} ({:.1}%)", similarity - 0.1046, (similarity - 0.1046) / 0.1046 * 100.0);

    // Manual dot product verification
    let manual_dot: f32 = emb1.iter().zip(&emb2).map(|(a, b)| a * b).sum();
    println!("\nManual dot product: {:.4}", manual_dot);

    // Check if embeddings are suspiciously similar
    let same_sign_count = emb1.iter().zip(&emb2)
        .filter(|(a, b)| a.signum() == b.signum())
        .count();
    println!("\nSame sign: {}/{} ({:.1}%)",
        same_sign_count,
        emb1.len(),
        same_sign_count as f32 / emb1.len() as f32 * 100.0
    );

    // For truly unrelated text, we'd expect ~50% same sign (random)
    // High percentage suggests embeddings aren't diverse enough
}

#[test]
#[ignore] // Run with: cargo test compare_similar_vs_dissimilar -- --ignored --nocapture
fn compare_similar_vs_dissimilar() {
    println!("\n=== Comparing Similar vs Dissimilar Pairs ===");

    // High similarity pair
    let high1 = TEST_MODEL.encode_single("The new movie is awesome").unwrap();
    let high2 = TEST_MODEL.encode_single("The new movie is so great").unwrap();
    let high_sim = cosine_similarity(&high1, &high2);
    let high_same_sign = high1.iter().zip(&high2)
        .filter(|(a, b)| a.signum() == b.signum())
        .count();

    println!("\nHigh Similarity Pair:");
    println!("  Similarity: {:.4} (expected 0.8939)", high_sim);
    println!("  Same sign: {}/384 ({:.1}%)", high_same_sign, high_same_sign as f32 / 384.0 * 100.0);

    // Low similarity pair
    let low1 = TEST_MODEL.encode_single("The weather is lovely today").unwrap();
    let low2 = TEST_MODEL.encode_single("He drove to the stadium").unwrap();
    let low_sim = cosine_similarity(&low1, &low2);
    let low_same_sign = low1.iter().zip(&low2)
        .filter(|(a, b)| a.signum() == b.signum())
        .count();

    println!("\nLow Similarity Pair:");
    println!("  Similarity: {:.4} (expected 0.1046)", low_sim);
    println!("  Same sign: {}/384 ({:.1}%)", low_same_sign, low_same_sign as f32 / 384.0 * 100.0);

    println!("\nKey Question:");
    println!("  Is there enough difference in 'same sign %' between high and low similarity?");
    println!("  High: {:.1}%", high_same_sign as f32 / 384.0 * 100.0);
    println!("  Low:  {:.1}%", low_same_sign as f32 / 384.0 * 100.0);
    println!("  Diff: {:.1}%", (high_same_sign - low_same_sign) as f32 / 384.0 * 100.0);
}
