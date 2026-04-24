//! Wiremock tests for [`OpenAICompatibleEmbeddingBackend`]. Covers
//! both the `OpenAI` and `Ollama` flavours since they share the wire.

use basilisk_embeddings::{
    EmbeddingError, EmbeddingInput, EmbeddingProvider, OpenAICompatibleEmbeddingBackend,
};
use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_flavour_sends_bearer_and_parses_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer sk-x"))
        .and(body_partial_json(serde_json::json!({
            "model": "text-embedding-3-large",
            "input": ["hello"],
            "encoding_format": "float"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "index": 0, "embedding": [0.1_f32, 0.2, 0.3] }],
            "usage": { "prompt_tokens": 5, "total_tokens": 5 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = OpenAICompatibleEmbeddingBackend::with_full_config(
        basilisk_embeddings::Provider::OpenAi,
        &server.uri(),
        "sk-x",
        "text-embedding-3-large",
        None,
        None,
    )
    .unwrap();
    let out = backend
        .embed(&[EmbeddingInput::document("hello")])
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].vector, vec![0.1, 0.2, 0.3]);
    assert_eq!(out[0].input_tokens, 5);
}

#[tokio::test]
async fn ollama_flavour_omits_authorization_header() {
    // Wiremock's matching is additive; asserting no `authorization`
    // header goes via a route that *only* matches when the header
    // is absent. We do that by making the success route require
    // `authorization: bearer ...` and confirming we get a 404
    // (wiremock default) when the backend doesn't send one.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "index": 0, "embedding": [1.0_f32] }],
            "usage": {}
        })))
        .mount(&server)
        .await;

    let backend = OpenAICompatibleEmbeddingBackend::ollama(Some(server.uri()), None).unwrap();
    // The mock above is the ONLY responder. Since the backend omits
    // the authorization header, it won't match, so wiremock returns
    // 404 and our backend classifies that as BadInput (HTTP 4xx).
    let err = backend
        .embed(&[EmbeddingInput::document("hi")])
        .await
        .unwrap_err();
    assert!(matches!(err, EmbeddingError::BadInput(_)), "{err:?}");
}

#[tokio::test]
async fn ollama_succeeds_when_server_accepts_no_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "index": 0, "embedding": [0.5_f32, 0.6] }],
            "usage": { "prompt_tokens": 2 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = OpenAICompatibleEmbeddingBackend::ollama(Some(server.uri()), None).unwrap();
    let out = backend
        .embed(&[EmbeddingInput::document("hi")])
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].vector, vec![0.5, 0.6]);
    // Only `prompt_tokens` set; parser takes the max.
    assert_eq!(out[0].input_tokens, 2);
}

#[tokio::test]
async fn rate_limit_propagates_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "9")
                .set_body_string("too fast"),
        )
        .mount(&server)
        .await;
    let backend = OpenAICompatibleEmbeddingBackend::with_full_config(
        basilisk_embeddings::Provider::OpenAi,
        &server.uri(),
        "sk-x",
        "text-embedding-3-large",
        None,
        None,
    )
    .unwrap();
    let err = backend
        .embed(&[EmbeddingInput::document("x")])
        .await
        .unwrap_err();
    match err {
        EmbeddingError::RateLimited { retry_after } => {
            assert_eq!(retry_after.map(|d| d.as_secs()), Some(9));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}
