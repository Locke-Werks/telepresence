fn main() {
    #[cfg(feature = "gui")]
    {
        println!("cargo::rerun-if-changed=src/video_sink.h");
        use cxx_qt_build::{CxxQtBuilder, QmlFile, QmlModule};
        let files = vec![QmlFile::from("qml/Main.qml"), QmlFile::from("qml/SmoothWheel.qml")];
        CxxQtBuilder::new_qml_module(QmlModule::new("TelePresence").qml_files(files))
            .qt_module("Quick")
            .qt_module("QuickControls2")
            .qt_module("Multimedia")
            .include_dir("src")
            .cpp_file("src/video_sink.cpp")
            .files(["src/bridge.rs"])
            .build();
    }
}
