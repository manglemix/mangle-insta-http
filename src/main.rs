use std::{
    fs::read_to_string,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{ws::Message, Path, State, WebSocketUpgrade},
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use bearer::BearerAuth;
use clap::Parser;
use notify::Watcher;
use pyo3::{intern, types::PyModule, PyObject, Python, ToPyObject};
use regex::RegexSet;
use serde::Deserialize;
use tokio::{select, time::interval};
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

const SYNCHRONIZE_SCRIPT_TIME: Duration = Duration::from_millis(1000);
const WS_CLASS: &str = "
from queue import Queue

class WebSocket:
    def __init__(self):
        self.send_msgs = Queue(3)
        self.recv_msgs = Queue()
    
    def recv_msg(self):
        return self.recv_msgs.get()
    
    def send_msg(self, msg):
        if msg is None:
            raise ValueError(\"Cannot send None in WebSocket\")
        self.send_msgs.put(msg)
";

#[derive(Clone, Copy)]
struct AppState {
    request_handler: &'static PyObject,
    ws_init: &'static PyObject,
    ws_class: &'static PyObject,
}

#[axum::debug_handler]
async fn default_handler(
    method: Method,
    Path(path): Path<String>,
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Vec<u8>) {
    Python::with_gil(|py| {
        let body = if let Ok(body) = String::from_utf8(body.to_vec()) {
            body.to_object(py)
        } else {
            body.to_object(py)
        };
        let object = state
            .request_handler
            .call1(py, (method.as_str(), path, body))
            .expect("Call error");

        let status_code;
        let bytes;

        if let Ok((code, body)) = object.extract::<(u16, Vec<u8>)>(py) {
            status_code = code;
            bytes = body;
        } else if let Ok((code, body)) = object.extract::<(u16, String)>(py) {
            status_code = code;
            bytes = body.into_bytes();
        } else {
            panic!("Invalid response body")
        }

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
    let (result, py_ws) = Python::with_gil(|py| {
        let py_ws = state.ws_class.call0(py).expect("Ws class init failed");
        (
            state
                .ws_init
                .call1(py, (path, py_ws.clone()))
                .expect("Calling ws_init")
                .is_true(py)
                .expect("Checking truthiness of ws_init result"),
            py_ws,
        )
    });
    if !result {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    ws.on_upgrade(|mut ws| async move {
        let py_ws_clone = py_ws.clone();
        let (msg_sender, mut msg_receiver) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || {
            Python::with_gil(|py| {
                let send_msgs = py_ws_clone
                    .getattr(py, intern!(py, "send_msgs"))
                    .expect("Getting send_msgs");
                loop {
                    let msg = send_msgs
                        .call_method0(py, intern!(py, "get"))
                        .expect("Popping message");

                    if msg.is_none(py) {
                        break;
                    }

                    let msg = if let Ok(msg) = msg.extract::<String>(py) {
                        Message::Text(msg)
                    } else if let Ok(msg) = msg.extract::<Vec<u8>>(py) {
                        Message::Binary(msg)
                    } else {
                        panic!("Invalid send message")
                    };

                    if msg_sender.blocking_send(msg).is_err() {
                        break;
                    }
                }
            })
        });

        let mut refcount_interval = interval(Duration::from_millis(200));

        loop {
            let msg = select! {
                _ = refcount_interval.tick() => {
                    if Python::with_gil(|py| {
                        py_ws.get_refcnt(py) == 2
                    }) {
                        break
                    }
                    continue
                }

                msg = msg_receiver.recv() => {
                    if ws.send(msg.unwrap()).await.is_err() {
                        break
                    }
                    continue
                }

                res = ws.recv() => {
                    let Some(res) = res else { break };
                    let Ok(msg) = res else {continue };
                    msg
                }
            };

            Python::with_gil(|py| {
                let msg = match msg {
                    Message::Text(msg) => msg.to_object(py),
                    Message::Binary(msg) => msg.to_object(py),
                    _ => return,
                };

                py_ws
                    .getattr(py, intern!(py, "recv_msgs"))
                    .expect("Getting recv_msgs")
                    .call_method1(py, intern!(py, "put"), (msg,))
                    .expect("Appending recv_msgs");
            });
        }

        Python::with_gil(|py| {
            py_ws
                .getattr(py, intern!(py, "send_msgs"))
                .expect("Getting send_msgs")
                .call_method1(py, intern!(py, "put"), (py.None(),))
                .expect("Appending send_msgs");
        });
    })
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = read_to_string(&args.config).expect("Reading config file");
    let config: Config = toml::from_str(&config).expect("Parsing config file");

    let cors_methods = config
        .cors_methods
        .into_iter()
        .map(|x| Method::from_str(&x).expect("Invalid CORS Method"))
        .collect::<Vec<_>>();
    let cors_origins = config
        .cors_origins
        .into_iter()
        .map(|x| HeaderValue::from_str(&x).expect("Invalid CORS Origin"))
        .collect::<Vec<_>>();
    let public_paths = RegexSet::new(config.public_paths)
        .ok()
        .unwrap_or_else(RegexSet::empty);

    let (file_change_sender, mut file_change_receiver) = tokio::sync::mpsc::channel::<()>(1);
    let last_modification = Arc::new(Mutex::new(Instant::now()));
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => match event.kind {
                notify::EventKind::Create(_) => {}
                notify::EventKind::Modify(_) => {}
                _ => return,
            },
            Err(_) => return,
        }

        let file_change_sender = file_change_sender.clone();
        let last_modification = last_modification.clone();
        *last_modification.lock().unwrap() = Instant::now();

        std::thread::spawn(move || {
            std::thread::sleep(SYNCHRONIZE_SCRIPT_TIME);
            if last_modification.lock().unwrap().elapsed() < SYNCHRONIZE_SCRIPT_TIME {
                return;
            }
            let _ = file_change_sender.try_send(());
        });
    })
    .expect("Setting up file watcher");
    watcher
        .watch(
            &std::path::Path::new(&config.program),
            notify::RecursiveMode::NonRecursive,
        )
        .expect("Watching script");

    macro_rules! err_wait {
        ($result: expr, $msg: literal) => {
            match $result {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("{}: {e:?}", $msg);
                    file_change_receiver.recv().await;
                    eprintln!("Reloading script\n");
                    continue;
                }
            }
        };
    }

    let api_token = config.api_token.map(|x| HeaderValue::from_str(&x).expect("API Token invalid"));
    let bind_address = config.bind_address.parse().expect("Invalid bind address");

    loop {
        let program = err_wait!(read_to_string(&config.program), "Error reading python file");

        let (request_handler, ws_init, ws_class, module) = err_wait!(
            Python::with_gil::<_, pyo3::PyResult<_>>(|py| {
                let module = PyModule::from_code(py, &program, &config.program, "mangle_http")?;
                Ok((
                    module.getattr("handle_request")?.to_object(py),
                    module.getattr("ws_init")?.to_object(py),
                    PyModule::from_code(py, WS_CLASS, "ws.py", "ws")?
                        .getattr("WebSocket")?
                        .to_object(py),
                    module.to_object(py),
                ))
            }),
            "Error parsing script"
        );

        let mut app = Router::new()
            .route("/ws/*path", get(ws_default_handler))
            .route(
                "/ws",
                get(|ws, state| ws_default_handler(ws, Path(String::new()), state)),
            )
            .route("/*path", get(default_handler))
            .route(
                "/",
                get(|method, state, body| {
                    default_handler(method, Path(String::new()), state, body)
                }),
            )
            .route("/*path", post(default_handler))
            .route(
                "/",
                post(|method, state, body| {
                    default_handler(method, Path(String::new()), state, body)
                }),
            )
            .with_state(AppState {
                request_handler: Box::leak(Box::new(request_handler)),
                ws_init: Box::leak(Box::new(ws_init)),
                ws_class: Box::leak(Box::new(ws_class)),
            })
            .layer(
                ServiceBuilder::new()
                    .layer(CompressionLayer::new())
                    .layer(TraceLayer::new_for_http())
                    .layer(
                        CorsLayer::new()
                            .allow_methods(cors_methods.clone())
                            .allow_origin(cors_origins.clone()),
                    ),
            );
        if let Some(api_token) = api_token.clone() {
            app = app.layer(RequireAuthorizationLayer::custom(BearerAuth::new(
                api_token,
                public_paths.clone(),
            )));
        }

        axum::Server::bind(&bind_address)
            .serve(app.into_make_service())
            .with_graceful_shutdown(async {
                Python::with_gil(|py| {
                    let Ok(init) = module.getattr(py, "init") else {
                        return;
                    };
                    if let Err(e) = init.call0(py) {
                        eprintln!("Error calling init from Python program: {e:?}");
                    }
                });
                file_change_receiver.recv().await;
                eprintln!("Reloading script\n")
            })
            .await
            .unwrap();
    }
}
