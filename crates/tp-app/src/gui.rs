use std::path::PathBuf;
use std::sync::OnceLock;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

pub fn config_dir() -> PathBuf {
    CONFIG_DIR.get().cloned().unwrap_or_else(crate::store::config_dir)
}

pub fn run(config_dir: PathBuf) {
    let _ = CONFIG_DIR.set(config_dir);
    let _guard = runtime().enter();
    let mut app = QGuiApplication::new();
    if let Some(mut app) = app.as_mut() {
        app.as_mut().set_application_name(&QString::from("TelePresence"));
        app.as_mut().set_organization_name(&QString::from("Locke Werks"));
        app.as_mut().set_application_version(&QString::from(env!("CARGO_PKG_VERSION")));
    }
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/TelePresence/qml/Main.qml"));
    }
    let code = app.as_mut().map(|app| app.exec()).unwrap_or(1);
    // QML objects (and with them the controller's cancel tokens) go first,
    // while the runtime is still alive to wind the core down.
    drop(engine);
    drop(app);
    std::process::exit(code);
}
