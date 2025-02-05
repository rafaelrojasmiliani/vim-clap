mod context;
mod manager;

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;

use anyhow::Result;
use futures::Future;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::stdio_server::impls::initialize;
use crate::stdio_server::rpc::Call;
use crate::stdio_server::types::ProviderId;
use crate::stdio_server::MethodCall;

pub use self::context::{SessionContext, SourceScale};
pub use self::manager::SessionManager;

static BACKGROUND_JOBS: Lazy<Arc<Mutex<HashSet<u64>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashSet::default())));

pub fn spawn_singleton_job(
    task_future: impl Future<Output = ()> + Send + Sync + 'static,
    job_id: u64,
) {
    if register_job_successfully(job_id) {
        tokio::spawn(async move {
            task_future.await;
            note_job_is_finished(job_id)
        });
    }
}

pub fn register_job_successfully(job_id: u64) -> bool {
    let mut background_jobs = BACKGROUND_JOBS.lock();
    if background_jobs.contains(&job_id) {
        false
    } else {
        background_jobs.insert(job_id);
        true
    }
}

pub fn note_job_is_finished(job_id: u64) {
    let mut background_jobs = BACKGROUND_JOBS.lock();
    background_jobs.remove(&job_id);
}

pub type SessionId = u64;

fn process_source_scale(source_scale: SourceScale, context: &SessionContext) {
    if let Some(total) = source_scale.total() {
        let method = "s:set_total_size";
        utility::println_json_with_length!(total, method);
    }

    if let Some(lines) = source_scale.initial_lines(100) {
        printer::decorate_lines(lines, context.display_winwidth as usize, context.icon)
            .print_on_session_create();
    }

    context.set_source_scale(source_scale);
}

#[async_trait::async_trait]
pub trait ClapProvider: Debug + Send + Sync + 'static {
    fn session_context(&self) -> &SessionContext;

    async fn on_create(&mut self, _call: Call) {
        const TIMEOUT: Duration = Duration::from_millis(300);

        let context = self.session_context();

        // TODO: blocking on_create for the swift providers like `tags`.
        match tokio::time::timeout(TIMEOUT, initialize(context)).await {
            Ok(scale_result) => match scale_result {
                Ok(scale) => process_source_scale(scale, context),
                Err(e) => tracing::error!(?e, "Error occurred on creating session"),
            },
            Err(_) => {
                // The initialization was not super fast.
                tracing::debug!(timeout = ?TIMEOUT, "Did not receive value in time");

                match context.provider_id.as_str() {
                    "grep" | "live_grep" => {
                        let rg_cmd =
                            crate::command::grep::RgTokioCommand::new(context.cwd.to_path_buf());
                        let job_id = utility::calculate_hash(&rg_cmd);
                        spawn_singleton_job(
                            async move {
                                let _ = rg_cmd.create_cache().await;
                            },
                            job_id,
                        );
                    }
                    _ => {
                        // TODO: Note arbitrary shell command and use par_dyn_run later.
                    }
                }
            }
        }
    }

    async fn on_move(&mut self, msg: MethodCall) -> Result<()>;

    async fn on_typed(&mut self, msg: MethodCall) -> Result<()>;

    /// Sets the running signal to false, in case of the forerunner thread is still working.
    fn handle_terminate(&self, session_id: u64) {
        let context = self.session_context();
        context.state.is_running.store(false, Ordering::SeqCst);
        tracing::debug!(
          session_id,
            provider_id = %context.provider_id,
            "Session terminated",
        );
    }
}

#[derive(Debug)]
pub struct Session {
    pub session_id: u64,
    /// Each provider session can have its own message processing logic.
    pub provider: Box<dyn ClapProvider>,
    pub event_recv: tokio::sync::mpsc::UnboundedReceiver<ProviderEvent>,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    OnTyped(MethodCall),
    OnMove(MethodCall),
    Create(Call),
    Terminate,
}

impl ProviderEvent {
    /// Simplified display of session event.
    pub fn short_display(&self) -> Cow<'_, str> {
        match self {
            Self::OnTyped(msg) => format!("OnTyped, msg_id: {}", msg.id).into(),
            Self::OnMove(msg) => format!("OnMove, msg_id: {}", msg.id).into(),
            Self::Create(_) => "Create".into(),
            Self::Terminate => "Terminate".into(),
        }
    }
}

impl Session {
    pub fn new(
        session_id: u64,
        provider: Box<dyn ClapProvider>,
    ) -> (Self, UnboundedSender<ProviderEvent>) {
        let (session_sender, session_receiver) = tokio::sync::mpsc::unbounded_channel();

        let session = Session {
            session_id,
            provider,
            event_recv: session_receiver,
        };

        (session, session_sender)
    }

    pub fn start_event_loop(mut self) {
        tokio::spawn(async move {
            if self.provider.session_context().debounce {
                self.run_event_loop_with_debounce().await;
            } else {
                self.run_event_loop_without_debounce().await;
            }
        });
    }

    async fn run_event_loop_with_debounce(mut self) {
        // https://github.com/denoland/deno/blob/1fb5858009f598ce3f917f9f49c466db81f4d9b0/cli/lsp/diagnostics.rs#L141
        //
        // Debounce timer delay. 150ms between keystrokes is about 45 WPM, so we
        // want something that is longer than that, but not too long to
        // introduce detectable UI delay; 200ms is a decent compromise.
        //
        // Add extra 50ms delay.
        const DELAY: Duration = Duration::from_millis(200 + 50);
        // If the debounce timer isn't active, it will be set to expire "never",
        // which is actually just 1 year in the future.
        const NEVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

        tracing::debug!(
            session_id = self.session_id,
            provider_id = %self.provider.session_context().provider_id,
            "Spawning a new session task",
        );

        let mut pending_on_typed = None;

        let debounce_timer = tokio::time::sleep(NEVER);
        tokio::pin!(debounce_timer);

        loop {
            tokio::select! {
                maybe_event = self.event_recv.recv() => {
                    match maybe_event {
                        Some(event) => {
                            tracing::debug!(event = ?event.short_display(), "Received an event");

                            match event {
                                ProviderEvent::Terminate => self.provider.handle_terminate(self.session_id),
                                ProviderEvent::Create(call) => self.provider.on_create(call).await,
                                ProviderEvent::OnMove(msg) => {
                                    if let Err(err) = self.provider.on_move(msg).await {
                                        tracing::error!(?err, "Error processing ProviderEvent::OnMove");
                                    }
                                }
                                ProviderEvent::OnTyped(msg) => {
                                    pending_on_typed.replace(msg);
                                    debounce_timer.as_mut().reset(Instant::now() + DELAY);
                                }
                            }
                          }
                          None => break, // channel has closed.
                      }
                }
                _ = debounce_timer.as_mut(), if pending_on_typed.is_some() => {
                    let msg = pending_on_typed.take().expect("Checked as Some above; qed");
                    debounce_timer.as_mut().reset(Instant::now() + NEVER);

                    if let Err(err) = self.provider.on_typed(msg).await {
                        tracing::error!(?err, "Error processing ProviderEvent::OnTyped");
                    }
                }
            }
        }
    }

    async fn run_event_loop_without_debounce(mut self) {
        while let Some(event) = self.event_recv.recv().await {
            tracing::debug!(event = ?event.short_display(), "Received an event");

            match event {
                ProviderEvent::Create(call) => self.provider.on_create(call).await,
                ProviderEvent::Terminate => self.provider.handle_terminate(self.session_id),
                ProviderEvent::OnMove(msg) => {
                    if let Err(err) = self.provider.on_move(msg).await {
                        tracing::debug!(?err, "Error processing ProviderEvent::OnMove");
                    }
                }
                ProviderEvent::OnTyped(msg) => {
                    if let Err(err) = self.provider.on_typed(msg).await {
                        tracing::debug!(?err, "Error processing ProviderEvent::OnTyped");
                    }
                }
            }
        }
    }
}
