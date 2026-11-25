//! One ChatGPT request attempt with transport, HTTP and stream retry classification.

use super::*;

impl Client {
    /// One request attempt. Transport failures, transient HTTP responses, and
    /// stream-reported `server_is_overloaded` or `server_error` are eligible
    /// for retry; `retry_reason` records the retry metadata for those.
    pub(super) async fn attempt(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
        retry_reason: &mut Option<Retry>,
        rejected_access_token: &mut Option<String>,
        observer: &dyn otto_core::diagnostics::Observer,
    ) -> ProviderSettlement {
        if let Err(error) = check_running(control) {
            return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
        }
        let http = match &self.http {
            Ok(http) => http,
            Err(error) => {
                return ProviderSettlement::failed(
                    ProviderError::Other(error.clone()),
                    0,
                    EffectCertainty::NotStarted,
                );
            }
        };
        let credentials =
            match await_control(control, self.tokens.token(control.cancellation_token())).await {
                Ok(Ok(credentials)) => credentials,
                Ok(Err(_)) => {
                    if let Err(error) = check_running(control) {
                        return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
                    }
                    return ProviderSettlement::failed(
                        ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
                        0,
                        EffectCertainty::NotStarted,
                    );
                }
                Err(error) => {
                    return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
                }
            };
        let access_token = credentials.access_token;
        if access_token.trim().is_empty() || self.account_id.trim().is_empty() {
            return ProviderSettlement::failed(
                ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
                0,
                EffectCertainty::NotStarted,
            );
        }

        let redactor = Redactor::new(&[access_token.clone(), self.account_id.clone()]);
        if !redactor.allows_dynamic_content() {
            return ProviderSettlement::failed(
                ProviderError::Other(REQUEST_FAILED.to_owned()),
                0,
                EffectCertainty::NotStarted,
            );
        }

        let payload = match serde_json::to_vec(&build_request_with(request, self.hosted_web_search))
        {
            Ok(payload) => payload,
            Err(error) => {
                return ProviderSettlement::failed(
                    ProviderError::Other(format!("encode responses request: {error}")),
                    0,
                    EffectCertainty::NotStarted,
                );
            }
        };
        if let Err(error) = check_running(control) {
            return chatgpt_failure(error, 0, EffectCertainty::NotStarted, control);
        }

        observer.activity(otto_core::diagnostics::Phase::Sending, 0, None);
        let send = http
            .post(format!("{}/responses", self.base_url))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("chatgpt-account-id", &self.account_id)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("OpenAI-Beta", BETA_HEADER)
            .header("originator", ORIGINATOR)
            .body(payload)
            .send();
        let response = match await_control(control, send).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if !error.is_builder() && !error.is_redirect() {
                    *retry_reason = Some(Retry {
                        reason: if error.is_timeout() {
                            "request timeout"
                        } else {
                            TRANSPORT_RETRY_REASON
                        },
                        retry_after: None,
                    });
                }
                if let Err(error) = check_running(control) {
                    return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                }
                return ProviderSettlement::failed(
                    ProviderError::Other(REQUEST_FAILED.to_owned()),
                    1,
                    EffectCertainty::Unknown,
                );
            }
            Err(error) => return chatgpt_failure(error, 1, EffectCertainty::Unknown, control),
        };

        let status = response.status().as_u16();
        observer.activity(otto_core::diagnostics::Phase::Headers, 0, Some(status));
        if status == 401 {
            *rejected_access_token = Some(access_token);
            return ProviderSettlement::failed(
                ProviderError::Other(AUTHORIZATION_FAILED.to_owned()),
                1,
                EffectCertainty::Unknown,
            );
        }
        if !(200..300).contains(&status) {
            let retry = transient_http_retry(status, response.headers());
            let details = match read_http_error(response, control).await {
                Ok(details) => details,
                Err(error) => return chatgpt_failure(error, 1, EffectCertainty::Unknown, control),
            };
            if details.policy_capacity {
                return ProviderSettlement::failed(
                    ProviderError::PolicyCapacityUnavailable,
                    1,
                    EffectCertainty::Completed,
                );
            }
            if matches!(status, 400 | 404)
                && let Some(path) = &self.catalog_cache_path
            {
                let _guard = self.catalog_cache_lock.lock().await;
                super::super::catalog_cache::invalidate(path);
            }
            if let Some(retry) = retry {
                *retry_reason = Some(retry);
                return ProviderSettlement::failed(
                    ProviderError::Other(format!("chatgpt responses HTTP {status}")),
                    1,
                    EffectCertainty::Unknown,
                );
            }
            let message = if status == 400 && request.request_remote_compaction {
                details.compaction_bad_request()
            } else {
                format!("chatgpt responses HTTP {status}")
            };
            return ProviderSettlement::failed(
                ProviderError::Other(message),
                1,
                EffectCertainty::Completed,
            );
        }

        let mut events = EventRedactor::new(&redactor);
        let mut assembler =
            StreamAssembler::with_remote_compaction(request.request_remote_compaction, "chatgpt");
        let mut body = Box::pin(response.bytes_stream());
        let mut deadline = tokio::time::Instant::now() + self.model_idle_timeout;
        loop {
            let chunk =
                match await_control(control, tokio::time::timeout_at(deadline, body.try_next()))
                    .await
                {
                    Ok(Ok(Ok(Some(chunk)))) => chunk,
                    Ok(Ok(Ok(None))) => break,
                    Ok(Err(_elapsed)) => {
                        *retry_reason = Some(Retry {
                            reason: MODEL_IDLE_RETRY_REASON,
                            retry_after: None,
                        });
                        return ProviderSettlement::failed(
                            ProviderError::Other(REQUEST_FAILED.to_owned()),
                            1,
                            EffectCertainty::Unknown,
                        );
                    }
                    Ok(Ok(Err(error))) => {
                        if !error.is_builder() && !error.is_redirect() {
                            *retry_reason = Some(Retry {
                                reason: if error.is_timeout() {
                                    "stream read timeout"
                                } else {
                                    TRANSPORT_RETRY_REASON
                                },
                                retry_after: None,
                            });
                        }
                        if let Err(error) = check_running(control) {
                            return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                        }
                        return ProviderSettlement::failed(
                            ProviderError::Other(REQUEST_FAILED.to_owned()),
                            1,
                            EffectCertainty::Unknown,
                        );
                    }
                    Err(error) => {
                        return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
                    }
                };
            observer.activity(otto_core::diagnostics::Phase::Receiving, chunk.len(), None);
            let mut progressed = false;
            let pushed = assembler.push(&chunk, &mut |event| {
                progressed = true;
                events.emit(event, &mut *emit)
            });
            if progressed {
                deadline = tokio::time::Instant::now() + self.model_idle_timeout;
            }
            if let Err(error) = pushed {
                if let Err(stop) = check_running(control) {
                    return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                }
                if matches!(error, StreamError::PolicyCapacityUnavailable) {
                    return ProviderSettlement::failed(
                        ProviderError::PolicyCapacityUnavailable,
                        1,
                        EffectCertainty::Completed,
                    );
                }
                *retry_reason = stream_retry(&error);
                let certainty = if retry_reason.is_some() {
                    EffectCertainty::Unknown
                } else {
                    EffectCertainty::Completed
                };
                return ProviderSettlement::failed(
                    ProviderError::Other(redactor.redact_string(&error.to_string())),
                    1,
                    certainty,
                );
            }
            if assembler.is_done() {
                let response = match assembler.finish(&mut |event| events.emit(event, &mut *emit)) {
                    Ok(response) => response,
                    Err(error) => {
                        if let Err(stop) = check_running(control) {
                            return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                        }
                        return ProviderSettlement::failed(
                            ProviderError::Other(redactor.redact_string(&error.to_string())),
                            1,
                            EffectCertainty::Completed,
                        );
                    }
                };
                events.flush(&mut *emit);
                return ProviderSettlement::succeeded(
                    Response {
                        message: redact_message(&redactor, response.message),
                        ..response
                    },
                    1,
                );
            }
            if let Err(error) = check_running(control) {
                return chatgpt_failure(error, 1, EffectCertainty::Unknown, control);
            }
        }

        let response = match assembler.finish(&mut |event| events.emit(event, &mut *emit)) {
            Ok(response) => response,
            Err(error) => {
                if let Err(stop) = check_running(control) {
                    return chatgpt_failure(stop, 1, EffectCertainty::Unknown, control);
                }
                if matches!(error, StreamError::PolicyCapacityUnavailable) {
                    return ProviderSettlement::failed(
                        ProviderError::PolicyCapacityUnavailable,
                        1,
                        EffectCertainty::Completed,
                    );
                }
                *retry_reason = stream_retry(&error);
                let certainty = if retry_reason.is_some() {
                    EffectCertainty::Unknown
                } else {
                    EffectCertainty::Completed
                };
                return ProviderSettlement::failed(
                    ProviderError::Other(redactor.redact_string(&error.to_string())),
                    1,
                    certainty,
                );
            }
        };
        events.flush(&mut *emit);
        ProviderSettlement::succeeded(
            Response {
                message: redact_message(&redactor, response.message),
                ..response
            },
            1,
        )
    }
}
