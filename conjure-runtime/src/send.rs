// Copyright 2020 Palantir Technologies, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::connect::proxy::ProxyConfig;
use crate::errors::{ThrottledError, TimeoutError, UnavailableError};
use crate::{
    node_selector, Body, BodyError, Client, ClientState, HyperBody, Request, ResetTrackingBody,
    Response,
};
use conjure_error::Error;
use futures::future;
use hyper::header::{
    HeaderValue, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, PROXY_AUTHORIZATION,
};
use hyper::http::header::RETRY_AFTER;
use hyper::{HeaderMap, StatusCode};
use rand::Rng;
use std::error::Error as _;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::time;
use url::Url;
use witchcraft_log::info;
use zipkin::TraceContext;

pub(crate) async fn send(client: &Client, request: Request<'_>) -> Result<Response, Error> {
    let client_state = client.shared.state.load_full();
    let mut state = State {
        request,
        client,
        client_state: &client_state,
        deadline: Instant::now() + client_state.request_timeout,
        nodes: client_state.node_selector.iter(),
        attempt: 0,
    };

    let span = zipkin::next_span()
        .with_name(&format!(
            "conjure-runtime: {} {}",
            state.request.method, state.request.pattern
        ))
        .detach();

    time::timeout(client_state.request_timeout, span.bind(state.send()))
        .await
        .unwrap_or_else(|_| Err(Error::internal_safe(TimeoutError(()))))
}

struct State<'a, 'b> {
    request: Request<'b>,
    client: &'a Client,
    client_state: &'a ClientState,
    deadline: Instant,
    nodes: node_selector::Iter<'a>,
    attempt: u32,
}

impl<'a, 'b> State<'a, 'b> {
    async fn send(&mut self) -> Result<Response, Error> {
        let mut body = self.request.body.take();

        loop {
            let span = zipkin::next_span()
                .with_name(&format!("conjure-runtime: attempt {}", self.attempt))
                .detach();
            let attempt = self.send_attempt(body.as_mut().map(|p| p.as_mut()));
            let (error, retry_after) = match span.bind(attempt).await? {
                AttemptOutcome::Ok(response) => return Ok(response),
                AttemptOutcome::Retry { error, retry_after } => (error, retry_after),
            };

            self.prepare_for_retry(body.as_mut().map(|p| p.as_mut()), error, retry_after)
                .await?;
        }
    }

    async fn send_attempt(
        &mut self,
        body: Option<Pin<&mut ResetTrackingBody<dyn Body + Sync + Send + 'b>>>,
    ) -> Result<AttemptOutcome, Error> {
        let node = match self.nodes.next() {
            Some(node) => node,
            None => {
                return Err(Error::internal_safe("unable to select a node for request")
                    .with_safe_param("service", &self.client.shared.service));
            }
        };

        let start = Instant::now();
        let resp = match self.send_raw(body, &node.url).await {
            Ok(response) => {
                let elapsed = start.elapsed();
                node.host_metrics.update(response.status(), elapsed);
                self.client.shared.response_timer.update(elapsed);

                if response.status().is_success() {
                    Ok(AttemptOutcome::Ok(response))
                } else if response.status() == StatusCode::TOO_MANY_REQUESTS {
                    let retry_after = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse().ok())
                        .map(Duration::from_secs);

                    if self.client.propagate_qos_errors() {
                        match retry_after {
                            Some(backoff) => {
                                Err(Error::throttle_for_safe("propagating 429", backoff))
                            }
                            None => Err(Error::throttle_safe("propagating 429")),
                        }
                    } else {
                        Ok(AttemptOutcome::Retry {
                            error: Error::internal_safe(ThrottledError(())),
                            retry_after,
                        })
                    }
                } else if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                    self.nodes.prev_failed();

                    if self.client.propagate_qos_errors() {
                        Err(Error::unavailable_safe("propagating 503"))
                    } else {
                        Ok(AttemptOutcome::Retry {
                            error: Error::internal_safe(UnavailableError(())),
                            retry_after: None,
                        })
                    }
                } else {
                    self.nodes.prev_failed();

                    Err(response
                        .into_error(self.client.propagate_service_errors())
                        .await)
                }
            }
            Err(error) => {
                node.host_metrics.update_io_error();
                self.client.shared.error_meter.mark(1);
                self.nodes.prev_failed();

                Ok(AttemptOutcome::Retry {
                    error,
                    retry_after: None,
                })
            }
        };

        match resp {
            Ok(AttemptOutcome::Ok(response)) => Ok(AttemptOutcome::Ok(response)),
            Ok(AttemptOutcome::Retry { error, retry_after }) => Ok(AttemptOutcome::Retry {
                error: error.with_safe_param("url", node.url.as_str()),
                retry_after,
            }),
            Err(error) => Err(error.with_safe_param("url", node.url.as_str())),
        }
    }

    async fn send_raw(
        &mut self,
        body: Option<Pin<&mut ResetTrackingBody<dyn Body + Sync + Send + 'b>>>,
        url: &Url,
    ) -> Result<Response, Error> {
        let headers_span = zipkin::next_span()
            .with_name("conjure-runtime: wait-for-headers")
            .detach();

        let headers = self.new_headers(headers_span.context(), &body);
        let (body, writer) = HyperBody::new(body);
        let request = self.new_request(headers, url, body);

        let (body_result, response_result) = headers_span
            .bind(future::join(
                writer.write(),
                self.client_state.client.request(request),
            ))
            .await;

        let response = match (body_result, response_result) {
            (Ok(()), Ok(response)) => response,
            (Ok(()), Err(e)) => return Err(Error::internal_safe(e)),
            (Err(e), Ok(response)) => {
                info!(
                    "body write reported an error on a successful request",
                    error: e
                );
                response
            }
            (Err(body), Err(hyper)) => return Err(self.deconflict_errors(body, hyper)),
        };

        let body_span = zipkin::next_span()
            .with_name("conjure-runtime: wait-for-body")
            .detach();
        Response::new(response, self.deadline, body_span)
    }

    fn new_headers(
        &self,
        context: TraceContext,
        body: &Option<Pin<&mut ResetTrackingBody<dyn Body + Sync + Send + 'b>>>,
    ) -> HeaderMap {
        let mut headers = self.request.headers.clone();
        headers.remove(CONNECTION);
        headers.remove(HOST);
        headers.remove(PROXY_AUTHORIZATION);
        headers.remove(CONTENT_LENGTH);
        headers.remove(CONTENT_TYPE);
        http_zipkin::set_trace_context(context, &mut headers);

        if let Some(body) = body {
            if let Some(length) = body.content_length() {
                let value = HeaderValue::from_str(&length.to_string()).unwrap();
                headers.insert(CONTENT_LENGTH, value);
            }
            headers.insert(CONTENT_TYPE, body.content_type());
        }

        headers
    }

    fn new_request(
        &self,
        mut headers: HeaderMap,
        url: &Url,
        hyper_body: HyperBody,
    ) -> hyper::Request<HyperBody> {
        let mut url = self.build_url(url);

        match &self.client_state.proxy {
            ProxyConfig::Http(config) => {
                if url.scheme() == "http" {
                    if let Some(credentials) = &config.credentials {
                        headers.insert(PROXY_AUTHORIZATION, credentials.clone());
                    }
                }
            }
            ProxyConfig::Mesh(config) => {
                let host = url.host_str().unwrap();
                let host = match url.port() {
                    Some(port) => format!("{}:{}", host, port),
                    None => host.to_string(),
                };
                let host = HeaderValue::from_str(&host).unwrap();
                headers.insert(HOST, host);
                url.set_host(Some(config.host_and_port.host())).unwrap();
                url.set_port(Some(config.host_and_port.port())).unwrap();
            }
            ProxyConfig::Direct => {}
        }

        let mut request = hyper::Request::new(hyper_body);
        *request.method_mut() = self.request.method.clone();
        *request.uri_mut() = url.as_str().parse().unwrap();
        *request.headers_mut() = headers;
        request
    }

    fn build_url(&self, url: &Url) -> Url {
        let mut url = url.clone();
        let mut params = self.request.params.clone();

        assert!(
            self.request.pattern.starts_with('/'),
            "pattern must start with `/`"
        );
        // make sure to skip the leading `/` to avoid an empty path segment
        for segment in self.request.pattern[1..].split('/') {
            match self.parse_param(segment) {
                Some(name) => match params.remove(name) {
                    Some(ref values) if values.len() != 1 => {
                        panic!("path segment parameter {} had multiple values", name);
                    }
                    Some(value) => {
                        url.path_segments_mut().unwrap().push(&value[0]);
                    }
                    None => panic!("path segment parameter {} had no values", name),
                },
                None => {
                    url.path_segments_mut().unwrap().push(segment);
                }
            }
        }

        for (k, vs) in &params {
            for v in vs {
                url.query_pairs_mut().append_pair(k, v);
            }
        }

        url
    }

    fn parse_param<'c>(&self, segment: &'c str) -> Option<&'c str> {
        if segment.starts_with('{') && segment.ends_with('}') {
            Some(&segment[1..segment.len() - 1])
        } else {
            None
        }
    }

    // An error in the body write will cause an error on the hyper side, and vice versa.
    // To pick the right one, we see if the hyper error was due the body write aborting or not.
    fn deconflict_errors(&self, body_error: Error, hyper_error: hyper::Error) -> Error {
        if hyper_error.source().map_or(false, |e| e.is::<BodyError>()) {
            body_error
        } else {
            Error::internal_safe(hyper_error)
        }
    }

    async fn prepare_for_retry(
        &mut self,
        body: Option<Pin<&mut ResetTrackingBody<dyn Body + Sync + Send + 'b>>>,
        error: Error,
        retry_after: Option<Duration>,
    ) -> Result<(), Error> {
        self.attempt += 1;
        if self.attempt >= self.client_state.max_num_retries {
            info!("exceeded retry limits");
            return Err(error);
        }

        if !self.request.idempotent {
            info!("unable to retry non-idempotent request");
            return Err(error);
        }

        if let Some(body) = body {
            if body.needs_reset() && !body.reset().await {
                info!("unable to reset body when retrying request");
                return Err(error);
            }
        }

        let backoff = match retry_after {
            Some(backoff) => backoff,
            None => {
                let scale = 1 << self.attempt;
                let max = self.client_state.backoff_slot_size * scale;
                rand::thread_rng().gen_range(Duration::from_secs(0), max)
            }
        };

        let _span = zipkin::next_span()
            .with_name("conjure-runtime: backoff-with-jitter")
            .detach();

        time::delay_for(backoff).await;

        Ok(())
    }
}

enum AttemptOutcome {
    Ok(Response),
    Retry {
        error: Error,
        retry_after: Option<Duration>,
    },
}
