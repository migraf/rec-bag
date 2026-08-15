mod support;

#[tokio::test(flavor = "multi_thread")]
async fn generated_large_library_harness_covers_all_workloads() {
    let result = support::run_suite(support::BenchConfig::test())
        .await
        .expect("tiny generated benchmark profile should complete");

    assert_eq!(result.measurements.len(), 6);
    assert_eq!(result.paginated_tracks, 12);
    assert_eq!(result.reconciliation_inputs, 36);
    assert_eq!(result.claimed_jobs, 4);
    assert!(result.snapshot_bytes > 1_000);
    assert_eq!(support::stable_id("track:1"), support::stable_id("track:1"));
}
