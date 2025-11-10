mod common;

use common::{cosine_similarity, SIMILARITY_FIXTURES, TEST_MODEL};

#[test]
fn test_similarity_against_fixtures() {
    // Test all reference cases from fixtures/similarity-reference.json
    let fixtures = &*SIMILARITY_FIXTURES;

    let mut passed = 0;
    let mut failed = 0;
    let mut failure_details = Vec::new();

    for test_case in &fixtures.test_cases {
        let emb1 = TEST_MODEL.encode_single(&test_case.text1)
            .unwrap_or_else(|_| panic!("Failed to encode text1 for {}", test_case.name));
        let emb2 = TEST_MODEL.encode_single(&test_case.text2)
            .unwrap_or_else(|_| panic!("Failed to encode text2 for {}", test_case.name));

        let actual_similarity = cosine_similarity(&emb1, &emb2);

        let diff = (actual_similarity - test_case.expected_similarity).abs();
        let within_tolerance = diff <= test_case.tolerance;

        if within_tolerance {
            passed += 1;
            println!(
                "✓ {} ({}): {:.4} (expected {:.4} ± {:.4})",
                test_case.name,
                test_case.category,
                actual_similarity,
                test_case.expected_similarity,
                test_case.tolerance
            );
        } else {
            failed += 1;
            let error_pct = (diff / test_case.expected_similarity * 100.0);
            println!(
                "✗ {} ({}): {:.4} (expected {:.4} ± {:.4}, diff {:.4} / {:.1}%)",
                test_case.name,
                test_case.category,
                actual_similarity,
                test_case.expected_similarity,
                test_case.tolerance,
                diff,
                error_pct
            );
            failure_details.push(format!(
                "\n  {} ({}):\n    Expected: {:.4} ± {:.4}\n    Actual:   {:.4}\n    Diff:     {:.4} ({:.1}%)",
                test_case.name,
                test_case.category,
                test_case.expected_similarity,
                test_case.tolerance,
                actual_similarity,
                diff,
                error_pct
            ));
        }
    }

    println!("\n{}/{} tests passed", passed, passed + failed);

    if !failure_details.is_empty() {
        panic!(
            "Similarity fixture validation failed: {}/{} tests passed\n\nFailures:{}",
            passed,
            passed + failed,
            failure_details.join("")
        );
    }
}
