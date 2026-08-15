mod support;

use support::{run_suite, BenchConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let result = async {
        let config = BenchConfig::from_env()?;
        run_suite(config).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("large-library benchmark failed: {error}");
        std::process::exit(1);
    }
}
