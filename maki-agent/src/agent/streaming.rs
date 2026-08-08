use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_providers::provider::Provider;
use maki_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use maki_providers::stats;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions, StreamResponse};
use maki_storage::id::SessionRef;
use serde_json::Value;
use tracing::warn;

use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

/// Returns when the first token landed, so the caller can bill the rest of the
/// stream to throughput instead of blending the wait into it.
async fn forward_provider_events(
    prx: flume::Receiver<ProviderEvent>,
    event_tx: &EventSender,
) -> (Option<Instant>, Option<String>) {
    let mut first_token = None;
    let mut upstream = None;
    while let Ok(pe) = prx.recv_async().await {
        // Progress events say the request was accepted, not that the model
        // started answering, so they must not count as the first token.
        if first_token.is_none()
            && matches!(
                pe,
                ProviderEvent::TextDelta { .. } | ProviderEvent::ThinkingDelta { .. }
            )
        {
            first_token = Some(Instant::now());
        }
        let ae = match pe {
            // Bookkeeping, not something the user asked to see.
            ProviderEvent::Upstream { name } => {
                upstream = Some(name);
                continue;
            }
            ProviderEvent::TextDelta { text } => AgentEvent::TextDelta { text },
            ProviderEvent::ThinkingDelta { text } => AgentEvent::ThinkingDelta { text },
            ProviderEvent::ToolUseStart { id, name } => AgentEvent::ToolPending { id, name },
            ProviderEvent::PromptProgress {
                processed,
                total,
                cache,
            } => AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            },
        };
        if event_tx.send(ae).is_err() {
            break;
        }
    }
    (first_token, upstream)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &EventSender,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
) -> Result<StreamResponse, AgentError> {
    let opts = opts.clamped(model);
    let messages = maki_providers::adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut retry = RetryState::new();
    let slug: Arc<str> = Arc::clone(&model.provider);
    loop {
        let started = Instant::now();
        let (ptx, prx) = flume::unbounded();
        let forwarder = smol::spawn({
            let event_tx = event_tx.clone();
            async move { forward_provider_events(prx, &event_tx).await }
        });
        let result = futures_lite::future::race(
            provider.stream_message(model, messages, system, tools, &ptx, opts, session_id),
            async {
                cancel.cancelled().await;
                Err(AgentError::Cancelled)
            },
        )
        .await;
        drop(ptx);
        let (first_token, upstream) = forwarder.await;
        // Both the broker and the upstream it chose, so `openrouter` stays
        // comparable to other providers while the upstream rows say which of
        // them was actually fast.
        let keys: Vec<String> = std::iter::once(slug.to_string())
            .chain(upstream.map(|u| stats::upstream_key(&slug, &u)))
            .collect();
        match result {
            Ok(r) => {
                let stream = first_token.map_or(Duration::ZERO, |t| t.elapsed());
                for key in &keys {
                    if let Some(t) = first_token {
                        stats::record_first_token(key, t.duration_since(started));
                    }
                    stats::record_success(key, r.usage.output, stream);
                }
                return Ok(r);
            }
            // A cancel is the user changing their mind, not the provider failing.
            Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
            Err(e) if e.is_retryable() => {
                if e.should_rotate_key()
                    && let Ok(true) = provider.rotate_key().await
                {
                    warn!("rotated API key after error: {e}");
                }
                let (attempt, delay) = retry.next_delay();
                if matches!(e, AgentError::Timeout { .. }) && attempt > MAX_TIMEOUT_RETRIES {
                    return Err(e);
                }
                let delay_ms = delay.as_millis() as u64;
                warn!(attempt, delay_ms, error = %e, "retryable, will retry");
                event_tx.send(AgentEvent::Retry {
                    attempt,
                    message: e.retry_message(),
                    delay_ms,
                })?;
                futures_lite::future::race(
                    async {
                        smol::Timer::after(delay).await;
                    },
                    cancel.cancelled(),
                )
                .await;
                if cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
            }
            Err(e) => {
                for key in &keys {
                    stats::record_error(key);
                }
                return Err(e);
            }
        }
    }
}
