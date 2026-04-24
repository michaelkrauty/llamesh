use futures::stream::{self, StreamExt};
use reqwest::Client;
use std::time::{Duration, Instant};

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary,
    load_cookbook_fixture, setup_mock_script, wait_for_ready,
};

/// Retry a request up to `max_retries` times on transient errors
async fn send_with_retry(
    client: &Client,
    url: &str,
    body: &serde_json::Value,
    max_retries: u32,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
        }
        match client.post(url).json(body).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                // Retry on connection/request errors, not on response errors
                if e.is_connect() || e.is_request() || e.is_timeout() {
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap())
}

#[tokio::test]
async fn test_stress_basic() {
    cleanup_procs("mock_server_stress.sh").await;
    cleanup_procs("config_stress.yaml").await;
    cleanup_by_port_range_pattern("132[0-4][0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "stress").await;
    let config_path = root.join("tests/config_stress.yaml");
    let cookbook_path = root.join("tests/cookbook_stress.yaml");
    let proxy_bin = llamesh_binary(&root);

    let config_content = format!(
        r#"
node_id: "stress-node"
listen_addr: "127.0.0.1:9096"
max_vram_mb: 1048576
max_sysmem_mb: 8192
default_model: "stress-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 8
  max_queue_size_per_model: 100
  max_instances_per_model: 4
  max_wait_in_queue_ms: 10000
llama_cpp_ports:
  ranges:
    - start: 13200
      end: 13249
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "{}"
  branch: "master"
  build_args: []
  build_command_args: []
  auto_update_interval_seconds: 0
  enabled: false
cluster:
  enabled: false
  peers: []
  gossip_interval_seconds: 5
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        mock_script.display()
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    // Use cookbook fixture
    let cookbook_content = load_cookbook_fixture("cookbook_stress").await;
    tokio::fs::write(&cookbook_path, cookbook_content)
        .await
        .unwrap();

    let mut proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy");

    assert!(
        wait_for_ready("http://127.0.0.1:9096").await,
        "Proxy failed to become ready"
    );

    // Start stress test
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let num_requests = 20; // Keep it small for CI/unit tests, user can increase
    let concurrency = 5;

    let start = Instant::now();

    let bodies = (0..num_requests).map(|i| {
        let client = client.clone();
        async move {
            let body = serde_json::json!({
                "model": "stress-model:default",
                "messages": [{"role": "user", "content": format!("Request {}", i)}],
                "stream": false
            });
            let start_req = Instant::now();
            let res = send_with_retry(
                &client,
                "http://127.0.0.1:9096/v1/chat/completions",
                &body,
                2, // up to 2 retries for transient failures
            )
            .await;
            (res, start_req.elapsed())
        }
    });

    let mut stream = stream::iter(bodies).buffer_unordered(concurrency);

    let mut success = 0;
    let mut total_latency = Duration::from_secs(0);

    while let Some((res, lat)) = stream.next().await {
        if let Ok(resp) = res {
            if resp.status().is_success() {
                success += 1;
                total_latency += lat;
            } else {
                println!("Request failed with status: {}", resp.status());
            }
        } else {
            println!("Request failed: {:?}", res.err());
        }
    }

    let duration = start.elapsed();
    println!("Total time: {:.2}s", duration.as_secs_f64());
    println!("Successful: {success}/{num_requests}");
    if success > 0 {
        println!(
            "Avg Latency: {:.2}ms",
            (total_latency.as_millis() as f64) / (success as f64)
        );
        println!(
            "Throughput: {:.2} req/s",
            (success as f64) / duration.as_secs_f64()
        );
    }

    // Accept 75% success rate to handle occasional timing issues in CI
    let min_success = (num_requests * 3) / 4;
    assert!(
        success >= min_success,
        "Expected at least 75% success ({min_success}/{num_requests}), got {success}/{num_requests}"
    );

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_stress.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
