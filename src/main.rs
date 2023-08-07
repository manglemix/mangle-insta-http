use std::{fs::read_to_string, ops::Deref, str::FromStr};

use axum::{
    body::Bytes,
    extract::{Path, State, WebSocketUpgrade, ws::Message},
    http::{HeaderValue, Method, StatusCode},
    routing::{get, post},
    Router, response::{Response, IntoResponse},
};
use bearer::BearerAuth;
use clap::Parser;
use pyo3::{types::PyModule, PyObject, Python, ToPyObject};
use regex::RegexSet;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower_http::{
    auth::RequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

mod bearer;

/// HTTP Server scripted with python
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the config file
    #[arg(default_value = "config.toml")]
    config: String,
}

#[derive(Deserialize)]
struct Config {
    program: String,
    bind_address: String,
    #[serde(default)]
    cors_methods: Vec<String>,
    #[serde(default)]
    cors_origins: Vec<String>,
    api_token: Option<String>,
    #[serde(default)]
    public_paths: Vec<String>,
}

#[derive(Clone, Copy)]
struct AppState {
    request_handler: &'static PyObject,
    ws_init: &'static PyObject,
}

#[axum::debug_handler]
async fn default_handler(
    method: Method,
    Path(path): Path<String>,
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Vec<u8>) {
    Python::with_gil(|py| {
        let object = if let Ok(body) = String::from_utf8(body.to_vec()) {
            state
                .request_handler
                .call1(py, (method.as_str(), path, body))
                .expect("Call error")
        } else {
            state
                .request_handler
                .call1(py, (method.as_str(), path, body.deref()))
                .expect("Call error")
        };

        let (status_code, bytes) = object
            .extract::<(u16, Vec<u8>)>(py)
            .expect("Invalid Python return type");
        (
            StatusCode::from_u16(status_code).expect("Invalid status code"),
            bytes,
        )
    })
}

#[axum::debug_handler]
async fn ws_default_handler(
    ws: WebSocketUpgrade,
    Path(path): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let (handler, is_none) = Python::with_gil(|py| {
        let handler = state.ws_init.call1(py, (path,)).expect("Calling ws_init").to_object(py);
        let is_none = handler.is_none(py);
        (handler, is_none)
    });
    if is_none {
        return (StatusCode::FORBIDDEN, "").into_response()
    }
    ws.on_upgrade(|mut ws| async move {
        loop {
            let Some(res) = ws.recv().await else { break };
            let Ok(msg) = res else { continue };

            let msg = Python::with_gil(|py| {
                let msg = match msg {
                    Message::Text(msg) => msg.to_object(py),
                    Message::Binary(msg) => msg.to_object(py),
                    _ => return None
                };

                let result = handler.call1(py, (msg,)).expect("Calling ws handler");
                if result.is_none(py) {
                    None
                } else if let Ok(msg) = result.extract::<String>(py) {
                    Some(Message::Text(msg))
                } else if let Ok(msg) = result.extract::<Vec<u8>>(py) {
                    Some(Message::Binary(msg))
                } else {
                    panic!("Invalid WS handler return type");
                }
            });

            if let Some(msg) = msg {
                if ws.send(msg).await.is_err() {
                    break
                }
            }
        }
    })
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = read_to_string(&args.config).expect("Reading config file");
    let config: Config = toml::from_str(&config).expect("Parsing config file");

    let program = read_to_string(&config.program).expect("Reading python file");
    let (request_handler, ws_init) = Python::with_gil(|py| {
        let module = PyModule::from_code(py, &program, &config.program, "mangle_http")
            .expect("Parsing py file");
        (
            module
                .getattr("handle_request")
                .expect("Getting handle_request function")
                .to_object(py),
            module
                .getattr("ws_init")
                .expect("Getting ws_init function")
                .to_object(py),
        )
    });

    let mut app = Router::new()
        .route("/ws/*path", get(ws_default_handler))
        .route(
            "/ws",
            get(|ws, state| ws_default_handler(ws, Path(String::new()), state)),
        )
        .route("/*path", get(default_handler))
        .route(
            "/",
            get(|method, state, body| default_handler(method, Path(String::new()), state, body)),
        )
        .route("/*path", post(default_handler))
        .route(
            "/",
            post(|method, state, body| default_handler(method, Path(String::new()), state, body)),
        )
        .with_state(AppState {
            request_handler: Box::leak(Box::new(request_handler)),
            ws_init: Box::leak(Box::new(ws_init)),
        })
        .layer(
            ServiceBuilder::new()
                .layer(CompressionLayer::new())
                .layer(TraceLayer::new_for_http())
                .layer(
                    CorsLayer::new()
                        .allow_methods(
                            config
                                .cors_methods
                                .into_iter()
                                .map(|x| Method::from_str(&x).expect("Invalid CORS Method"))
                                .collect::<Vec<_>>(),
                        )
                        .allow_origin(
                            config
                                .cors_origins
                                .into_iter()
                                .map(|x| HeaderValue::from_str(&x).expect("Invalid CORS Origin"))
                                .collect::<Vec<_>>(),
                        ),
                ),
        );
    if let Some(api_token) = config.api_token {
        app = app.layer(RequireAuthorizationLayer::custom(BearerAuth::new(
            HeaderValue::from_str(&api_token).expect("API Token invalid"),
            RegexSet::new(config.public_paths)
                .ok()
                .unwrap_or_else(RegexSet::empty),
        )));
    }

    axum::Server::bind(&config.bind_address.parse().expect("Invalid bind address"))
        .serve(app.into_make_service())
        .await
        .unwrap();
}
