//! The run API end to end: start, watch, stop.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use minact_studio::StudioServer;
use tower::ServiceExt;

fn workspace(ci: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temp workspace");
    let dir = tmp.path().join(".minact/workflows");
    std::fs::create_dir_all(&dir).expect("create workflow dir");
    std::fs::write(dir.join("ci.yml"), ci).expect("write workflow");
    tmp
}

async fn get(router: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn post(
    router: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("request");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Read the SSE body to completion. The stream closes itself when the run ends,
/// so this doubles as "wait for the run to finish".
async fn read_stream(router: &Router, uri: &str) -> String {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");

    let bytes = tokio::time::timeout(Duration::from_secs(30), response.into_body().collect())
        .await
        .expect("the stream must end when the run does")
        .expect("read body")
        .to_bytes();

    String::from_utf8_lossy(&bytes).into_owned()
}

async fn only_workflow_id(router: &Router) -> String {
    let (_, list) = get(router, "/api/workflows").await;
    list[0]["id"].as_str().expect("workflow id").to_string()
}

#[tokio::test]
async fn a_run_streams_its_events_and_records_the_result() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  setup:
    steps:
      - name: Greet
        run: echo hello-from-setup
  build:
    needs: [setup]
    steps:
      - name: Compile
        run: echo compiled
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (status, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(started["status"], "running");
    let run_id = started["id"].as_str().expect("run id").to_string();

    // Subscribing from 0 replays everything already emitted and then follows
    // the live feed, so a subscriber that arrives late misses nothing.
    let stream = read_stream(&router, &format!("/api/runs/{}/events?from=0", run_id)).await;

    assert!(stream.contains("hello-from-setup"), "stream: {}", stream);
    assert!(stream.contains("compiled"));
    assert!(stream.contains("event: end"));
    assert!(stream.contains("\"success\""));
    // Sequence numbers double as SSE ids so a reconnect can resume.
    assert!(stream.contains("id: 0"));

    let (status, detail) = get(&router, &format!("/api/runs/{}", run_id)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["status"], "success");
    assert!(detail["duration_ms"].as_i64().is_some());

    let jobs = detail["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0]["id"], "setup", "jobs come back in execution order");
    assert_eq!(jobs[0]["conclusion"], "success");
    assert_eq!(jobs[0]["steps"][0]["name"], "Greet");
    assert!(
        jobs[0]["steps"][0]["duration_ms"].as_i64().is_some(),
        "a finished step must report how long it took",
    );

    assert_eq!(detail["layers"][0][0], "setup");
    assert_eq!(detail["layers"][1][0], "build");
}

#[tokio::test]
async fn a_failing_run_reports_which_step_failed() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Fine
        run: echo ok
      - name: Broken
        run: exit 3
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (_, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;
    let run_id = started["id"].as_str().unwrap().to_string();

    read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;

    let (_, detail) = get(&router, &format!("/api/runs/{}", run_id)).await;
    assert_eq!(detail["status"], "failure");

    let steps = detail["jobs"][0]["steps"].as_array().expect("steps");
    assert_eq!(steps[0]["conclusion"], "success");
    assert_eq!(steps[1]["conclusion"], "failure");
}

#[tokio::test]
async fn cancelling_stops_a_run_in_progress() {
    let tmp = workspace(
        r#"
name: Slow
on: workflow_dispatch
jobs:
  wait:
    steps:
      - name: Sleep
        run: sleep 30
      - name: Never
        run: echo should-not-appear
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (_, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;
    let run_id = started["id"].as_str().unwrap().to_string();

    let cancelling = {
        let router = router.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            post(
                &router,
                &format!("/api/runs/{}/cancel", run_id),
                serde_json::json!({}),
            )
            .await
        })
    };

    let stream = read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;
    let (status, _) = cancelling.await.expect("cancel request");
    assert_eq!(status, StatusCode::OK);

    assert!(!stream.contains("should-not-appear"));

    let (_, detail) = get(&router, &format!("/api/runs/{}", run_id)).await;
    assert_eq!(detail["status"], "cancelled");
    assert_eq!(detail["jobs"][0]["conclusion"], "cancelled");
}

#[tokio::test]
async fn a_finished_run_replays_from_disk_after_a_restart() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Say
        run: echo persisted-line
"#,
    );

    let run_id = {
        let router = StudioServer::new(tmp.path()).router();
        let workflow_id = only_workflow_id(&router).await;
        let (_, started) = post(
            &router,
            "/api/runs",
            serde_json::json!({ "workflow_id": workflow_id }),
        )
        .await;
        let run_id = started["id"].as_str().unwrap().to_string();
        read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;
        run_id
    };

    // A brand new server, as if the process had been restarted.
    let restarted = StudioServer::new(tmp.path()).router();

    let (status, list) = get(&restarted, "/api/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], run_id);
    assert_eq!(list[0]["status"], "success");

    let (_, detail) = get(&restarted, &format!("/api/runs/{}", run_id)).await;
    assert_eq!(detail["jobs"][0]["steps"][0]["name"], "Say");

    // The events came back off disk, so the stream still works.
    let stream = read_stream(&restarted, &format!("/api/runs/{}/events", run_id)).await;
    assert!(stream.contains("persisted-line"), "stream: {}", stream);
    assert!(stream.contains("event: end"));
}

#[tokio::test]
async fn resuming_from_a_sequence_skips_what_the_client_already_has() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Say
        run: echo a-line
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (_, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;
    let run_id = started["id"].as_str().unwrap().to_string();
    read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;

    let (_, detail) = get(&router, &format!("/api/runs/{}", run_id)).await;
    let last_seq = detail["last_seq"].as_u64().expect("last_seq");
    assert!(last_seq > 0);

    let tail = read_stream(
        &router,
        &format!("/api/runs/{}/events?from={}", run_id, last_seq),
    )
    .await;

    assert!(tail.contains(&format!("id: {}", last_seq)));
    assert!(
        !tail.contains("id: 0"),
        "everything before the resume point must be skipped: {}",
        tail,
    );
}

#[tokio::test]
async fn starting_a_run_for_an_unknown_workflow_is_a_404() {
    let tmp = workspace("name: CI\non: push\njobs:\n  a:\n    steps:\n      - run: echo hi\n");
    let router = StudioServer::new(tmp.path()).router();

    let (status, body) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": "bm90LWhlcmU" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn watching_an_unknown_run_is_a_404() {
    let tmp = workspace("name: CI\non: push\njobs:\n  a:\n    steps:\n      - run: echo hi\n");
    let router = StudioServer::new(tmp.path()).router();

    let (status, _) = get(&router, "/api/runs/9999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_run_can_be_downloaded_as_plain_text() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Compile
        run: echo compiling
  check:
    needs: [build]
    steps:
      - name: Verify
        run: echo verifying
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (_, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;
    let run_id = started["id"].as_str().unwrap().to_string();
    read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/runs/{}/logs", run_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    assert!(response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains(&format!("minact-run-{}.log", run_id)),);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);

    assert!(text.starts_with(&format!("# minact run {}\n", run_id)));
    assert!(text.contains("status:   success"));
    assert!(text.contains("compiling"), "{}", text);
    assert!(text.contains("verifying"));

    // One job at a time, for when only half the run is interesting.
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/runs/{}/logs?job=build", run_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let only_build = String::from_utf8_lossy(&bytes);

    assert!(only_build.contains("compiling"));
    assert!(!only_build.contains("verifying"), "{}", only_build);
}

#[tokio::test]
async fn the_run_list_can_be_filtered() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  ok:
    steps:
      - run: echo fine
"#,
    );
    let dir = tmp.path().join(".minact/workflows");
    std::fs::write(
        dir.join("bad.yml"),
        "name: Bad\non: workflow_dispatch\njobs:\n  bad:\n    steps:\n      - run: exit 1\n",
    )
    .unwrap();

    let router = StudioServer::new(tmp.path()).router();
    let (_, workflows) = get(&router, "/api/workflows").await;
    let bad_id = workflows
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "Bad")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let ok_id = workflows
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "CI")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    for workflow_id in [&ok_id, &bad_id, &ok_id] {
        let (_, started) = post(
            &router,
            "/api/runs",
            serde_json::json!({ "workflow_id": workflow_id }),
        )
        .await;
        let id = started["id"].as_str().unwrap().to_string();
        read_stream(&router, &format!("/api/runs/{}/events", id)).await;
    }

    let (_, all) = get(&router, "/api/runs").await;
    assert_eq!(all.as_array().unwrap().len(), 3);

    let (_, failures) = get(&router, "/api/runs?status=failure").await;
    assert_eq!(failures.as_array().unwrap().len(), 1);
    assert_eq!(failures[0]["workflow_name"], "Bad");

    let (_, by_workflow) = get(&router, &format!("/api/runs?workflow={}", ok_id)).await;
    assert_eq!(by_workflow.as_array().unwrap().len(), 2);

    // Newest first, so a limit keeps the newest.
    let (_, latest) = get(&router, "/api/runs?limit=1").await;
    assert_eq!(latest.as_array().unwrap().len(), 1);
    assert_eq!(latest[0]["id"], "3");
}

#[tokio::test]
async fn artifacts_are_listed_and_downloadable() {
    let tmp = workspace(
        r#"
name: CI
on: workflow_dispatch
jobs:
  build:
    steps:
      - name: Make a file
        run: |
          mkdir -p dist
          echo "built output" > dist/app.txt
      - name: Upload
        uses: actions/upload-artifact@v4
        with:
          name: build-output
          path: ./dist
"#,
    );
    let router = StudioServer::new(tmp.path()).router();
    let workflow_id = only_workflow_id(&router).await;

    let (_, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": workflow_id }),
    )
    .await;
    let run_id = started["id"].as_str().unwrap().to_string();
    read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;

    let (status, artifacts) = get(&router, "/api/artifacts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(artifacts[0]["name"], "build-output");
    assert_eq!(artifacts[0]["files"][0]["path"], "app.txt");
    assert_eq!(artifacts[0]["files"][0]["previewable"], true);

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/artifacts/build-output/app.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "built output");

    // Studio serves this over HTTP, so a traversal has to be refused here too,
    // not only in the resolver's own tests.
    let (status, _) = get(&router, "/api/artifacts/build-output/..%2f..%2fsecret").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_extra_directory_can_be_browsed_and_run() {
    let tmp =
        workspace("name: CI\non: push\njobs:\n  a:\n    steps:\n      - run: echo built-in\n");

    // A folder of workflows that is not a project layout — minact would never
    // look here on its own.
    let examples = tmp.path().join("examples");
    std::fs::create_dir_all(&examples).unwrap();
    std::fs::write(
        examples.join("demo.yml"),
        "name: Demo\non: workflow_dispatch\njobs:\n  show:\n    steps:\n      - run: echo from-examples\n",
    )
    .unwrap();

    let router = StudioServer::new(tmp.path())
        .with_workflow_dirs(["examples"])
        .router();

    let (status, list) = get(&router, "/api/workflows").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    let demo = list
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "Demo")
        .expect("the extra directory is searched");
    assert_eq!(demo["path"], "examples/demo.yml");
    assert_eq!(demo["source"], "examples/");

    // And it is a workflow like any other: runnable, watchable, recorded.
    let (status, started) = post(
        &router,
        "/api/runs",
        serde_json::json!({ "workflow_id": demo["id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let run_id = started["id"].as_str().unwrap().to_string();
    let stream = read_stream(&router, &format!("/api/runs/{}/events", run_id)).await;
    assert!(stream.contains("from-examples"), "stream: {}", stream);

    let (_, detail) = get(&router, &format!("/api/runs/{}", run_id)).await;
    assert_eq!(detail["status"], "success");
    assert_eq!(detail["workflow_path"], "examples/demo.yml");
}
