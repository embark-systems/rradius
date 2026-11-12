//! What the tests of the RADIUS servers need: the log lines written while a test runs.

use std::sync::Arc;

/// What a subscriber wrote while a test ran.
#[derive(Clone, Default)]
pub struct CapturedLog(pub Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;

    fn make_writer(&'a self) -> CapturedLog {
        self.clone()
    }
}

/// What is logged while `run` runs.
pub async fn logged_during<F: std::future::Future>(run: F) -> String {
    let log = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).with_writer(log.clone()).finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    run.await;
    let text = log.0.lock().unwrap().clone();
    String::from_utf8(text).unwrap()
}

/// The shipped dictionary, for tests in other modules.
pub fn share_test_dictionary() -> Arc<crate::dictionary::Dictionary> {
    let mut dictionary = crate::dictionary::Dictionary::new();
    dictionary.load_from_file(super::dictionary()).unwrap();
    Arc::new(dictionary)
}
