//! One request round trip: sending, reading the response and mapping errors.

use super::*;

impl Client {
    /// One request/response round trip, including reading the whole stream.
    pub(super) async fn attempt(
        &self,
        ready: &Ready,
        payload: &[u8],
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
        observer: &dyn otto_core::diagnostics::Observer,
    ) -> Result<Response, AttemptError> {
        check_running(control).map_err(AttemptError::Sent)?;
        self.attempt_sent(ready, payload, emit, control, observer)
            .await
    }

    /// [`Self::attempt`] after the stop check: a connection that could not
    /// be established is [`AttemptError::NotSent`], and a transport failure
    /// after dispatch with nothing emitted yet is
    /// [`AttemptError::Interrupted`].
    async fn attempt_sent(
        &self,
        ready: &Ready,
        payload: &[u8],
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
        observer: &dyn otto_core::diagnostics::Observer,
    ) -> Result<Response, AttemptError> {
        observer.activity(otto_core::diagnostics::Phase::Sending, 0, None);
        let send = ready
            .http
            .post(format!("{}/chat/completions", ready.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(payload.to_vec())
            .send();
        let response = match await_control(control, send)
            .await
            .map_err(AttemptError::Sent)?
        {
            Ok(response) => response,
            Err(error) => {
                check_running(control).map_err(AttemptError::Sent)?;
                let message = format!("send chat completion request: {}", error_chain(&error));
                return Err(if error.is_connect() {
                    AttemptError::NotSent(ProviderError::Other(message))
                } else if is_transient_transport(&error) {
                    // The request may already be at the endpoint even though
                    // its response never arrived.
                    AttemptError::Interrupted(ProviderError::Other(message), TRANSPORT_RETRY_REASON)
                } else {
                    AttemptError::Sent(ProviderError::Other(message))
                });
            }
        };
        self.read_response(response, emit, control, observer).await
    }

    /// Reads a sent request's response: an error status, or the whole stream.
    ///
    /// A transport failure that happens before any event reached `emit` is
    /// [`AttemptError::Interrupted`], so the retry policy may repeat the
    /// attempt. Once a reasoning, text or tool-call delta has been handed to
    /// the caller nothing can be repeated, so the failure is
    /// [`AttemptError::Sent`].
    async fn read_response(
        &self,
        response: reqwest::Response,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
        observer: &dyn otto_core::diagnostics::Observer,
    ) -> Result<Response, AttemptError> {
        let status = response.status();
        observer.activity(
            otto_core::diagnostics::Phase::Headers,
            0,
            Some(status.as_u16()),
        );
        if !status.is_success() {
            return Err(AttemptError::Sent(
                self.error_response(status.as_u16(), response, control)
                    .await,
            ));
        }

        let mut assembler = StreamAssembler::new();
        let mut body = Box::pin(response.bytes_stream());
        let mut emitted = false;
        // Set by every event, cleared after each chunk to restart the idle
        // deadline. Shared by reference because `forward` stays borrowed
        // across the loop; atomic so the future stays `Send`.
        let progressed = std::sync::atomic::AtomicBool::new(false);
        let mut forward = |event: StreamEvent| {
            emitted = true;
            progressed.store(true, std::sync::atomic::Ordering::Relaxed);
            emit(event);
        };
        let mut deadline = tokio::time::Instant::now() + self.model_idle_timeout;
        while !assembler.is_done() {
            let chunk =
                match await_control(control, tokio::time::timeout_at(deadline, body.try_next()))
                    .await
                    .map_err(AttemptError::Sent)?
                {
                    Ok(Ok(Some(chunk))) => chunk,
                    Ok(Ok(None)) => break,
                    Err(_elapsed) => {
                        // The prefix makes the settlement `Unknown`: the endpoint
                        // may still be generating.
                        let error = ProviderError::Other(format!(
                            "read chat completion stream: no model output for {:?}",
                            self.model_idle_timeout
                        ));
                        return Err(if !emitted {
                            AttemptError::Interrupted(error, MODEL_IDLE_RETRY_REASON)
                        } else {
                            AttemptError::Sent(error)
                        });
                    }
                    Ok(Err(error)) => {
                        check_running(control).map_err(AttemptError::Sent)?;
                        let message =
                            format!("read chat completion stream: {}", error_chain(&error));
                        return Err(if !emitted && is_transient_transport(&error) {
                            AttemptError::Interrupted(
                                ProviderError::Other(message),
                                TRANSPORT_RETRY_REASON,
                            )
                        } else {
                            AttemptError::Sent(ProviderError::Other(message))
                        });
                    }
                };
            observer.activity(otto_core::diagnostics::Phase::Receiving, chunk.len(), None);
            {
                if let Err(error) = assembler.push(&chunk, &mut forward) {
                    check_running(control).map_err(AttemptError::Sent)?;
                    return Err(AttemptError::Sent(ProviderError::Other(error.to_string())));
                }
                if progressed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    deadline = tokio::time::Instant::now() + self.model_idle_timeout;
                }
                // Once a complete response has been received and validated, it
                // wins a simultaneous stop. Before then, the stop reason wins.
                if assembler.is_done() {
                    return match assembler.finish(&mut forward) {
                        Ok(response) => Ok(response),
                        Err(error) => {
                            check_running(control).map_err(AttemptError::Sent)?;
                            Err(AttemptError::Sent(ProviderError::Other(error.to_string())))
                        }
                    };
                }
                check_running(control).map_err(AttemptError::Sent)?;
            }
        }
        let result = assembler
            .finish(&mut forward)
            .map_err(|error| AttemptError::Sent(ProviderError::Other(error.to_string())));
        match result {
            Ok(response) => Ok(response),
            Err(error) => {
                check_running(control).map_err(AttemptError::Sent)?;
                Err(error)
            }
        }
    }

    /// Turns a non-2xx response into a failure, reading a bounded prefix of
    /// the body so a hostile endpoint cannot stream an unbounded error.
    async fn error_response(
        &self,
        status: u16,
        response: reqwest::Response,
        control: &dyn OperationControl,
    ) -> ProviderError {
        let body = match read_error_body(response, control).await {
            ErrorBody::Stopped(_) => {
                return ProviderError::Other(format!(
                    "OpenAI-compatible HTTP {status} (error body unavailable)"
                ));
            }
            ErrorBody::Unreadable => {
                return ProviderError::Other(format!(
                    "OpenAI-compatible HTTP {status} (error body unreadable)"
                ));
            }
            ErrorBody::Body(body) => body,
        };
        let safe = self.redact(&body);
        if let Some(overflow) = classify_overflow(status, &safe) {
            return ProviderError::Overflow(overflow);
        }
        ProviderError::Other(format!(
            "OpenAI-compatible HTTP {status}: {}",
            String::from_utf8_lossy(&safe).trim()
        ))
    }

    /// Redacts the API key from text that is about to leave the module.
    /// [`ProviderError::Overflow`] is returned untouched because it carries no
    /// provider text to redact.
    pub(super) fn safe_error(&self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Other(message) if !self.api_key.is_empty() => ProviderError::Other(
                String::from_utf8_lossy(&self.redact(message.as_bytes())).into_owned(),
            ),
            other => other,
        }
    }

    /// Replaces every occurrence of the API key. A key shorter than
    /// `[REDACTED]` is replaced by as many `*` as it has bytes, so the
    /// replacement never reveals the key's length by being longer than it.
    /// An empty key redacts nothing.
    fn redact(&self, body: &[u8]) -> Vec<u8> {
        if self.api_key.is_empty() {
            return body.to_vec();
        }
        let key = self.api_key.as_bytes();
        let stars = vec![b'*'; key.len()];
        let replacement: &[u8] = if REDACTED.len() > key.len() {
            &stars
        } else {
            REDACTED.as_bytes()
        };
        let mut out = Vec::with_capacity(body.len());
        let mut index = 0;
        while index < body.len() {
            if body[index..].starts_with(key) {
                out.extend_from_slice(replacement);
                index += key.len();
            } else {
                out.push(body[index]);
                index += 1;
            }
        }
        out
    }
}
