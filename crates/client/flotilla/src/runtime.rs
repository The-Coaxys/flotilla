//! The Tokio runtime that owns all networking; the UI stays on GPUI's executor.
pub fn start() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("flotilla-io")
        .enable_all()
        .build()
        .expect("cannot start the Tokio runtime")
}
