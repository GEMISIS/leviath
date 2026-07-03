//! Abstraction over I/O for the stage executor.
//!
//! Implementations:
//! - `ForegroundIO` (stdin/stdout) — used by `run_foreground`
//! - `WorkerIO` (file IPC) — used by `run_worker`
//! - `MockIO` (testing) — captures all output for assertions

use async_trait::async_trait;
use leviath_core::blueprint::StageResult;
use leviath_core::Stage;

use crate::runstate::RegionSnapshot;

/// Abstraction over I/O for the stage executor.
#[async_trait]
pub trait RunIO: Send {
    /// Called when entering a new stage
    async fn on_stage_enter(
        &mut self,
        stage: &Stage,
        visit_num: usize,
        provider: &str,
        model: &str,
    );

    /// Called when a stage completes
    async fn on_stage_complete(
        &mut self,
        stage_name: &str,
        result: &StageResult,
        next_stage: Option<&str>,
    );

    /// Display inference output text
    async fn on_output(&mut self, text: &str);

    /// Display token usage
    async fn on_tokens(&mut self, prompt: usize, completion: usize, cached: usize);

    /// Report a tool call and its result
    async fn on_tool_call(&mut self, tool_name: &str, tool_id: &str, result: &str);

    /// Get user input for interactive stages (returns None if not interactive)
    async fn get_user_input(&mut self, prompt: &str) -> Option<String>;

    /// Report an error
    async fn on_error(&mut self, error: &str);

    /// Report provider not configured
    async fn on_provider_missing(&mut self, provider: &str);

    /// Whether this is a background/worker context (affects snapshot writing etc.)
    fn is_background(&self) -> bool;

    /// Write a context snapshot (worker mode writes to disk, foreground is no-op)
    fn write_context_snapshot(&mut self, snapshot: &RegionSnapshot);
}

/// Reads a single line from `reader` and returns it trimmed, or `None` on a
/// read error (note: EOF is `Ok(0)`, not an error, so it yields
/// `Some(String::new())` -- preserved as-is from the original inline
/// implementation). Generic over `R` (rather than hardcoding real stdin)
/// purely so tests can drive it with an in-memory reader instead of blocking
/// on real process stdin.
fn get_user_input_from_reader<R: std::io::BufRead + ?Sized>(reader: &mut R) -> Option<String> {
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    Some(buf.trim().to_string())
}

/// Console I/O implementation: prints to stdout, reads from stdin.
/// Used by both foreground and worker modes at runtime.
pub struct ConsoleIO {
    reader: Box<dyn std::io::BufRead + Send>,
}

impl ConsoleIO {
    pub fn new() -> Self {
        Self {
            reader: Box::new(std::io::BufReader::new(std::io::stdin())),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_reader(r: impl std::io::BufRead + Send + 'static) -> Self {
        Self {
            reader: Box::new(r),
        }
    }
}

impl Default for ConsoleIO {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RunIO for ConsoleIO {
    async fn on_stage_enter(
        &mut self,
        _stage: &Stage,
        _visit_num: usize,
        _provider: &str,
        _model: &str,
    ) {
    }

    async fn on_stage_complete(
        &mut self,
        _stage_name: &str,
        _result: &StageResult,
        _next_stage: Option<&str>,
    ) {
    }

    async fn on_output(&mut self, text: &str) {
        print!("{}", text);
        use std::io::Write;
        std::io::stdout().flush().ok();
    }

    async fn on_tokens(&mut self, prompt: usize, completion: usize, _cached: usize) {
        println!("\n[Tokens: {} in, {} out]", prompt, completion);
    }

    async fn on_tool_call(&mut self, _tool_name: &str, _tool_id: &str, _result: &str) {}

    async fn get_user_input(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write;
        println!("{}", prompt);
        print!("You: ");
        std::io::stdout().flush().ok();
        get_user_input_from_reader(&mut *self.reader)
    }

    async fn on_error(&mut self, error: &str) {
        eprintln!("{}", error);
    }

    async fn on_provider_missing(&mut self, _provider: &str) {}

    fn is_background(&self) -> bool {
        false
    }

    fn write_context_snapshot(&mut self, _snapshot: &RegionSnapshot) {}
}

#[cfg(test)]
mod console_io_tests {
    use super::*;
    use leviath_core::blueprint::{ModelConfig, StageResult};
    use std::io::Cursor;

    fn make_stage(name: &str) -> Stage {
        Stage::new(
            name.to_string(),
            ModelConfig::new("anthropic".to_string(), "test".to_string()),
        )
    }

    #[tokio::test]
    async fn console_io_on_stage_enter_is_noop() {
        let mut io = ConsoleIO::new();
        let stage = make_stage("main");
        io.on_stage_enter(&stage, 0, "anthropic", "claude").await;
    }

    #[tokio::test]
    async fn console_io_on_stage_complete_is_noop() {
        let mut io = ConsoleIO::new();
        io.on_stage_complete("main", &StageResult::Success, Some("next"))
            .await;
        io.on_stage_complete("final", &StageResult::MaxIterations, None)
            .await;
    }

    #[tokio::test]
    async fn console_io_on_output_prints_and_flushes() {
        let mut io = ConsoleIO::new();
        io.on_output("hello").await;
    }

    #[tokio::test]
    async fn console_io_on_tokens_prints() {
        let mut io = ConsoleIO::new();
        io.on_tokens(100, 50, 25).await;
    }

    #[tokio::test]
    async fn console_io_on_tool_call_is_noop() {
        let mut io = ConsoleIO::new();
        io.on_tool_call("read_file", "tc-1", "contents").await;
    }

    #[tokio::test]
    async fn console_io_on_error_prints_to_stderr() {
        let mut io = ConsoleIO::new();
        io.on_error("something broke").await;
    }

    #[tokio::test]
    async fn console_io_on_provider_missing_is_noop() {
        let mut io = ConsoleIO::new();
        io.on_provider_missing("anthropic").await;
    }

    #[test]
    fn console_io_is_not_background() {
        let io = ConsoleIO::new();
        assert!(!io.is_background());
    }

    #[test]
    fn console_io_default_is_not_background() {
        let io = ConsoleIO::default();
        assert!(!io.is_background());
    }

    #[test]
    fn console_io_write_context_snapshot_is_noop() {
        let mut io = ConsoleIO::new();
        let snapshot = RegionSnapshot {
            name: "conversation".to_string(),
            kind: "sliding".to_string(),
            current_tokens: 5,
            max_tokens: 100,
            entries: vec![],
        };
        io.write_context_snapshot(&snapshot);
    }

    struct ErrorReader;
    impl std::io::Read for ErrorReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "test"))
        }
    }
    impl std::io::BufRead for ErrorReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "test"))
        }
        fn consume(&mut self, _: usize) {}
    }

    #[test]
    fn get_user_input_from_reader_read_error_returns_none() {
        let mut reader = ErrorReader;
        assert_eq!(get_user_input_from_reader(&mut reader), None);
    }

    #[test]
    fn error_reader_read_impl_returns_error() {
        use std::io::Read;
        let mut reader = ErrorReader;
        let mut buf = [0u8; 4];
        assert!(reader.read(&mut buf).is_err());
    }

    #[test]
    fn error_reader_consume_is_noop() {
        use std::io::BufRead;
        let mut reader = ErrorReader;
        reader.consume(4);
    }

    #[tokio::test]
    async fn console_io_get_user_input_reads_from_injected_reader() {
        let mut io = ConsoleIO::with_reader(std::io::Cursor::new(b"hello world\n".to_vec()));
        let result = io.get_user_input("Enter:").await;
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn get_user_input_from_reader_returns_trimmed_line() {
        let mut reader = Cursor::new(b"  hello world  \n".to_vec());
        assert_eq!(
            get_user_input_from_reader(&mut reader),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn get_user_input_from_reader_eof_returns_empty_string() {
        // EOF is `Ok(0)`, not an error, so this yields `Some("")` rather
        // than `None` -- matches the pre-existing inline behavior.
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert_eq!(get_user_input_from_reader(&mut reader), Some(String::new()));
    }

    #[test]
    fn get_user_input_from_reader_multiple_lines_reads_first_only() {
        let mut reader = Cursor::new(b"first\nsecond\n".to_vec());
        assert_eq!(
            get_user_input_from_reader(&mut reader),
            Some("first".to_string())
        );
        assert_eq!(
            get_user_input_from_reader(&mut reader),
            Some("second".to_string())
        );
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::VecDeque;

    /// Mock I/O implementation for testing. Captures all output for assertions.
    pub struct MockIO {
        pub stage_entries: Vec<String>,
        pub stage_completions: Vec<String>,
        pub outputs: Vec<String>,
        pub token_records: Vec<(usize, usize, usize)>,
        pub tool_calls: Vec<(String, String, String)>,
        pub errors: Vec<String>,
        pub provider_missing: Vec<String>,
        pub user_inputs: VecDeque<String>,
        pub snapshots: Vec<RegionSnapshot>,
        pub background: bool,
    }

    impl MockIO {
        pub fn new() -> Self {
            Self {
                stage_entries: Vec::new(),
                stage_completions: Vec::new(),
                outputs: Vec::new(),
                token_records: Vec::new(),
                tool_calls: Vec::new(),
                errors: Vec::new(),
                provider_missing: Vec::new(),
                user_inputs: VecDeque::new(),
                snapshots: Vec::new(),
                background: false,
            }
        }

        pub fn with_inputs(mut self, inputs: Vec<String>) -> Self {
            self.user_inputs = inputs.into();
            self
        }

        pub fn with_background(mut self, bg: bool) -> Self {
            self.background = bg;
            self
        }
    }

    impl Default for MockIO {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl RunIO for MockIO {
        async fn on_stage_enter(
            &mut self,
            stage: &Stage,
            visit_num: usize,
            provider: &str,
            model: &str,
        ) {
            self.stage_entries.push(format!(
                "{}:{}:{}:{}",
                stage.name, visit_num, provider, model
            ));
        }

        async fn on_stage_complete(
            &mut self,
            stage_name: &str,
            result: &StageResult,
            next_stage: Option<&str>,
        ) {
            self.stage_completions.push(format!(
                "{}:{:?}:{}",
                stage_name,
                result,
                next_stage.unwrap_or("none")
            ));
        }

        async fn on_output(&mut self, text: &str) {
            self.outputs.push(text.to_string());
        }

        async fn on_tokens(&mut self, prompt: usize, completion: usize, cached: usize) {
            self.token_records.push((prompt, completion, cached));
        }

        async fn on_tool_call(&mut self, tool_name: &str, tool_id: &str, result: &str) {
            self.tool_calls.push((
                tool_name.to_string(),
                tool_id.to_string(),
                result.to_string(),
            ));
        }

        async fn get_user_input(&mut self, _prompt: &str) -> Option<String> {
            self.user_inputs.pop_front()
        }

        async fn on_error(&mut self, error: &str) {
            self.errors.push(error.to_string());
        }

        async fn on_provider_missing(&mut self, provider: &str) {
            self.provider_missing.push(provider.to_string());
        }

        fn is_background(&self) -> bool {
            self.background
        }

        fn write_context_snapshot(&mut self, snapshot: &RegionSnapshot) {
            self.snapshots.push(snapshot.clone());
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use leviath_core::blueprint::{ModelConfig, StageResult};

        fn make_stage(name: &str) -> Stage {
            Stage::new(
                name.to_string(),
                ModelConfig::new("anthropic".to_string(), "test".to_string()),
            )
        }

        #[tokio::test]
        async fn mock_io_records_stage_entries() {
            let mut io = MockIO::new();
            let stage = make_stage("test-stage");
            io.on_stage_enter(&stage, 0, "anthropic", "claude").await;
            io.on_stage_enter(&stage, 1, "openai", "gpt-5").await;

            assert_eq!(io.stage_entries.len(), 2);
            assert!(io.stage_entries[0].contains("test-stage"));
            assert!(io.stage_entries[1].contains("1"));
        }

        #[tokio::test]
        async fn mock_io_records_outputs() {
            let mut io = MockIO::new();
            io.on_output("hello world").await;
            io.on_output("goodbye").await;

            assert_eq!(io.outputs, vec!["hello world", "goodbye"]);
        }

        #[tokio::test]
        async fn mock_io_returns_user_inputs_in_order() {
            let mut io = MockIO::new().with_inputs(vec!["first".to_string(), "second".to_string()]);

            assert_eq!(io.get_user_input("prompt").await, Some("first".to_string()));
            assert_eq!(
                io.get_user_input("prompt").await,
                Some("second".to_string())
            );
            assert_eq!(io.get_user_input("prompt").await, None);
        }

        #[tokio::test]
        async fn mock_io_records_errors() {
            let mut io = MockIO::new();
            io.on_error("something went wrong").await;
            assert_eq!(io.errors, vec!["something went wrong"]);
        }

        #[tokio::test]
        async fn mock_io_records_tool_calls() {
            let mut io = MockIO::new();
            io.on_tool_call("read_file", "tc-1", "file contents").await;
            assert_eq!(io.tool_calls.len(), 1);
            assert_eq!(io.tool_calls[0].0, "read_file");
        }

        #[tokio::test]
        async fn mock_io_records_stage_completions() {
            let mut io = MockIO::new();
            io.on_stage_complete("main", &StageResult::Success, Some("next"))
                .await;
            io.on_stage_complete("final", &StageResult::MaxIterations, None)
                .await;

            assert_eq!(io.stage_completions.len(), 2);
            assert!(io.stage_completions[0].contains("main"));
            assert!(io.stage_completions[1].contains("final"));
        }

        #[test]
        fn mock_io_background_flag() {
            let io = MockIO::new();
            assert!(!io.is_background());

            let io = MockIO::new().with_background(true);
            assert!(io.is_background());
        }

        #[tokio::test]
        async fn mock_io_records_tokens() {
            let mut io = MockIO::new();
            io.on_tokens(100, 50, 25).await;
            assert_eq!(io.token_records, vec![(100, 50, 25)]);
        }

        #[tokio::test]
        async fn mock_io_records_provider_missing() {
            let mut io = MockIO::new();
            io.on_provider_missing("anthropic").await;
            assert_eq!(io.provider_missing, vec!["anthropic"]);
        }

        #[test]
        fn mock_io_records_context_snapshot() {
            let mut io = MockIO::new();
            let snapshot = RegionSnapshot {
                name: "conversation".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 5,
                max_tokens: 100,
                entries: vec![],
            };
            io.write_context_snapshot(&snapshot);
            assert_eq!(io.snapshots.len(), 1);
            assert_eq!(io.snapshots[0].name, "conversation");
        }

        #[test]
        fn mock_io_default_matches_new() {
            let io = MockIO::default();
            assert!(io.outputs.is_empty());
            assert!(!io.is_background());
        }
    }
}
