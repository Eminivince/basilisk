//! Wiremock tests for [`VoyageBackend`]. No network, no keys.
//!
//! Mirrors the structure of `crates/llm/tests/anthropic_wiremock.rs`.

use basilisk_embeddings::{EmbeddingError, EmbeddingInput, EmbeddingProvider, VoyageBackend};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn backend(server: &MockServer) -> VoyageBackend {
    VoyageBackend::with_base_and_model(server.uri(), "sk-voyage-test", "voyage-code-3").unwrap()
}

#[tokio::test]
async fn embed_happy_path_returns_vectors_in_input_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer sk-voyage-test"))
        .and(body_partial_json(serde_json::json!({
            "model": "voyage-code-3",
            "input_type": "document",
            "input": ["hello world", "second doc"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            // Intentionally out of order — the parser reorders.
            "data": [
                { "object": "embedding", "index": 1, "embedding": [0.2_f32, 0.3, 0.4] },
                { "object": "embedding", "index": 0, "embedding": [0.1_f32, 0.2, 0.3] }
            ],
            "model": "voyage-code-3",
            "usage": { "total_tokens": 6 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let inputs = vec![
        EmbeddingInput::document("hello world"),
        EmbeddingInput::document("second doc"),
    ];
    let out = backend(&server).embed(&inputs).await.expect("ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].vector, vec![0.1, 0.2, 0.3]);
    assert_eq!(out[1].vector, vec![0.2, 0.3, 0.4]);
    // 6 total tokens / 2 inputs, ceil-divided = 3 each.
    assert_eq!(out[0].input_tokens, 3);
    assert_eq!(out[1].input_tokens, 3);
}

#[tokio::test]
async fn embed_query_batch_sends_input_type_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(body_partial_json(serde_json::json!({
            "input_type": "query"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                { "index": 0, "embedding": [1.0_f32, 2.0] }
            ],
            "usage": { "total_tokens": 1 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    backend(&server)
        .embed(&[EmbeddingInput::query("what is it")])
        .await
        .unwrap();
}

#[tokio::test]
async fn auth_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let err = backend(&server)
        .embed(&[EmbeddingInput::document("x")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbeddingError::AuthError(_)));
}

#[tokio::test]
async fn rate_limit_surfaces_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "11")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;
    let err = backend(&server)
        .embed(&[EmbeddingInput::document("x")])
        .await
        .unwrap_err();
    match err {
        EmbeddingError::RateLimited { retry_after } => {
            assert_eq!(retry_after.map(|d| d.as_secs()), Some(11));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_request_carries_status_and_body_fragment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(400).set_body_string("input too long"))
        .mount(&server)
        .await;
    let err = backend(&server)
        .embed(&[EmbeddingInput::document("x")])
        .await
        .unwrap_err();
    match err {
        EmbeddingError::BadInput(s) => assert!(s.contains("input too long"), "got {s}"),
        other => panic!("expected BadInput, got {other:?}"),
    }
}

#[tokio::test]
async fn server_5xx_classifies_as_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;
    let err = backend(&server)
        .embed(&[EmbeddingInput::document("x")])
        .await
        .unwrap_err();
    match err {
        EmbeddingError::ServerError { status, body } => {
            assert_eq!(status, 503);
            assert!(body.contains("upstream down"));
        }
        other => panic!("expected ServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn parse_error_when_response_shape_mismatches_expected_count() {
    // Server returns 1 embedding for a 2-input request — the
    // backend should reject rather than silently drop.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "index": 0, "embedding": [1.0_f32] } ],
            "usage": { "total_tokens": 1 }
        })))
        .mount(&server)
        .await;
    let err = backend(&server)
        .embed(&[EmbeddingInput::document("a"), EmbeddingInput::document("b")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbeddingError::ParseError(_)));
}
