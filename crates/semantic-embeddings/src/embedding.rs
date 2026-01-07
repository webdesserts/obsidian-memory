use anyhow::Result;

/// Compute cosine similarity between two embedding vectors
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() {
        anyhow::bail!("Vector dimensions must match: {} vs {}", a.len(), b.len());
    }

    if a.is_empty() {
        return Ok(0.0);
    }

    // Compute dot product
    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();

    // Compute magnitudes
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        return Ok(0.0);
    }

    Ok(dot_product / (magnitude_a * magnitude_b))
}

/// Find indices of top K most similar embeddings to query
///
/// Returns indices sorted by similarity (descending)
pub fn find_most_similar(query: &[f32], candidates: &[Vec<f32>], top_k: usize) -> Result<Vec<u32>> {
    if candidates.is_empty() {
        return Ok(vec![]);
    }

    // Compute similarities for all candidates
    let mut similarities: Vec<(usize, f32)> = candidates
        .iter()
        .enumerate()
        .map(|(idx, candidate)| {
            let sim = cosine_similarity(query, candidate).unwrap_or(0.0);
            (idx, sim)
        })
        .collect();

    // Sort by similarity descending
    similarities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top K
    let k = top_k.min(similarities.len());
    Ok(similarities[..k]
        .iter()
        .map(|(idx, _)| *idx as u32)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!(
            (sim - 1.0).abs() < 1e-6,
            "Identical vectors should have similarity 1.0"
        );
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!(
            sim.abs() < 1e-6,
            "Orthogonal vectors should have similarity 0.0"
        );
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!(
            (sim + 1.0).abs() < 1e-6,
            "Opposite vectors should have similarity -1.0"
        );
    }

    #[test]
    fn test_find_most_similar() {
        let query = vec![1.0, 0.0, 0.0];
        let candidates = vec![
            vec![1.0, 0.0, 0.0], // idx 0: similarity 1.0
            vec![0.0, 1.0, 0.0], // idx 1: similarity 0.0
            vec![0.7, 0.7, 0.0], // idx 2: similarity ~0.7
        ];

        let top_2 = find_most_similar(&query, &candidates, 2).unwrap();
        assert_eq!(
            top_2,
            vec![0, 2],
            "Should return indices of top 2 most similar"
        );
    }

    #[test]
    fn test_find_most_similar_empty() {
        let query = vec![1.0, 0.0];
        let candidates: Vec<Vec<f32>> = vec![];
        let result = find_most_similar(&query, &candidates, 5).unwrap();
        assert!(
            result.is_empty(),
            "Empty candidates should return empty result"
        );
    }
}
