use anyhow::{anyhow, bail, Result};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use url::Url;

mod bindings {
    wit_bindgen::generate!({
        world: "ai",
        generate_all,
    });
}

use bindings::{
    exports::wasmcloud::ai::streaming_handler::Guest,
    wasi::http::types::{Fields, IncomingResponse, Method, OutgoingRequest, Scheme},
};

struct Component;

impl Guest for Component {
    fn prompt_handle(prompt: String) -> String {
        executor::run(async move { handle_request(prompt).await })
    }
}

bindings::export!(Component with_types_in bindings);

async fn handle_request(prompt: String) -> String {
    eprintln!("[COMPONENT] Received prompt: {}", prompt);

    match openai_proxy(prompt).await {
        Ok(response) => {
            eprintln!("[COMPONENT] Got response from OpenAI API");

            let mut stream =
                executor::incoming_body(response.consume().expect("response should be consumable"));
            let mut collected_data = Vec::new();

            // Collect complete non-streaming response
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(data) => collected_data.extend_from_slice(&data),
                    Err(e) => {
                        eprintln!("[COMPONENT] Error receiving body: {e}");
                        return format!("Error collecting response: {}", e);
                    }
                }
            }

            eprintln!(
                "[COMPONENT] Response collected, {} bytes",
                collected_data.len()
            );

            // Convert to string
            let raw_response = match String::from_utf8(collected_data) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("[COMPONENT] UTF-8 error: {e}");
                    return format!("Error: Invalid UTF-8 response");
                }
            };

            // Parse JSON and extract output text for non-streaming response
            match parse_complete_response(&raw_response) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("[COMPONENT] JSON parse error: {e}");
                    raw_response // Fallback to raw JSON
                }
            }
        }
        Err(e) => {
            eprintln!("[COMPONENT] OpenAI request error: {e}");
            format!("Error: {}", e)
        }
    }
}

async fn openai_proxy(prompt: String) -> Result<IncomingResponse> {
    let base = "https://api.openai.com/v1/responses";

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| anyhow!("OPENAI_API_KEY environment variable not set"))?;

    let url: Url = Url::parse(base)?;

    // Build headers
    let headers = Fields::new();
    headers
        .append(&"content-type".to_string(), b"application/json")
        .map_err(|_| anyhow!("failed to set content-type"))?;
    headers
        .append(
            &"authorization".to_string(),
            format!("Bearer {}", api_key).as_bytes(),
        )
        .map_err(|_| anyhow!("failed to set authorization"))?;

    let outgoing_request = OutgoingRequest::new(headers);

    outgoing_request
        .set_method(&Method::Post)
        .map_err(|()| anyhow!("failed to set POST method"))?;

    let path_with_query = url.path().to_string();
    outgoing_request
        .set_path_with_query(Some(&path_with_query))
        .map_err(|()| anyhow!("failed to set path"))?;

    outgoing_request
        .set_scheme(Some(&match url.scheme() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            scheme => Scheme::Other(scheme.into()),
        }))
        .map_err(|()| anyhow!("failed to set scheme"))?;

    outgoing_request
        .set_authority(Some(&format!(
            "{}{}",
            url.host_str().unwrap_or(""),
            if let Some(port) = url.port() {
                format!(":{port}")
            } else {
                String::new()
            }
        )))
        .map_err(|()| anyhow!("failed to set authority"))?;

    // JSON payload with stream: false for complete response
    let json_request = format!(
        r#"{{
            "model": "gpt-4.1",
            "input": "{}",
            "stream": false
        }}"#,
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );

    // Send request body
    let mut body = executor::outgoing_body(outgoing_request.body().expect("body writable"));
    body.send(json_request.into_bytes()).await?;
    drop(body);

    // Send request
    let response = executor::outgoing_request_send(outgoing_request).await?;

    let status = response.status();
    if !(200..300).contains(&status) {
        bail!("HTTP {} from OpenAI", status);
    }

    Ok(response)
}

fn parse_complete_response(json_str: &str) -> Result<String> {
    let json: Value =
        serde_json::from_str(json_str).map_err(|e| anyhow!("Failed to parse JSON: {}", e))?;

    // Primary path: OpenAI Responses API format from your exact log output
    if let Some(output_array) = json.get("output") {
        if let Some(first_msg) = output_array.get(0usize) {
            // Use usize index for array access
            if let Some(content_array) = first_msg.get("content") {
                if let Some(content_item) = content_array.get(0usize) {
                    // Use usize index
                    // Direct "text" field check first (handles your exact format)
                    if let Some(text) = content_item.get("text") {
                        if let Some(text_str) = text.as_str() {
                            return Ok(text_str.to_string());
                        }
                    }
                    // Fallback to output_text.text
                    if let Some(output_text) = content_item.get("output_text") {
                        if let Some(text) = output_text.get("text") {
                            if let Some(text_str) = text.as_str() {
                                return Ok(text_str.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Debug: Log the structure if parsing fails
    eprintln!(
        "[COMPONENT] JSON keys: {:?}",
        json.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );

    bail!("No output text found in response")
}

// [Keep the entire executor module unchanged - it's the same as original]
mod executor {
    use crate::bindings::wasi::{
        http::{
            outgoing_handler,
            types::{
                self, FutureTrailers, IncomingBody, IncomingResponse, InputStream, OutgoingBody,
                OutgoingRequest, OutputStream,
            },
        },
        io::{self, streams::StreamError},
    };
    use anyhow::{anyhow, Error, Result};
    use futures::{future, sink, stream, Sink, Stream};
    use std::{
        cell::RefCell,
        future::Future,
        mem,
        rc::Rc,
        sync::{Arc, Mutex},
        task::{Context, Poll, Wake, Waker},
    };

    const READ_SIZE: u64 = 16 * 1024;

    static WAKERS: Mutex<Vec<(io::poll::Pollable, Waker)>> = Mutex::new(Vec::new());

    pub fn run<T>(future: impl Future<Output = T>) -> T {
        futures::pin_mut!(future);

        struct DummyWaker;
        impl Wake for DummyWaker {
            fn wake(self: Arc<Self>) {}
        }

        let waker = Arc::new(DummyWaker).into();

        loop {
            match future.as_mut().poll(&mut Context::from_waker(&waker)) {
                Poll::Pending => {
                    let mut new_wakers = Vec::new();
                    let wakers = mem::take::<Vec<_>>(&mut *WAKERS.lock().unwrap());
                    assert!(!wakers.is_empty());

                    let pollables = wakers
                        .iter()
                        .map(|(pollable, _)| pollable)
                        .collect::<Vec<_>>();
                    let mut ready = vec![false; wakers.len()];

                    for index in io::poll::poll(&pollables) {
                        ready[usize::try_from(index).unwrap()] = true;
                    }

                    for (ready, (pollable, waker)) in ready.into_iter().zip(wakers) {
                        if ready {
                            waker.wake();
                        } else {
                            new_wakers.push((pollable, waker));
                        }
                    }

                    *WAKERS.lock().unwrap() = new_wakers;
                }
                Poll::Ready(result) => break result,
            }
        }
    }

    pub fn outgoing_body(body: OutgoingBody) -> impl Sink<Vec<u8>, Error = Error> {
        struct Outgoing(Option<(OutputStream, OutgoingBody)>);

        impl Drop for Outgoing {
            fn drop(&mut self) {
                if let Some((stream, body)) = self.0.take() {
                    drop(stream);
                    OutgoingBody::finish(body, None).expect("outgoing-body.finish");
                }
            }
        }

        let stream = body.write().expect("response body should be writable");
        let pair = Rc::new(RefCell::new(Outgoing(Some((stream, body)))));

        sink::unfold((), move |(), chunk: Vec<u8>| {
            future::poll_fn({
                let mut offset = 0;
                let mut flushing = false;
                let pair = pair.clone();

                move |context| {
                    let pair = pair.borrow();
                    let (stream, _) = &pair.0.as_ref().unwrap();

                    loop {
                        match stream.check_write() {
                            Ok(0) => {
                                WAKERS
                                    .lock()
                                    .unwrap()
                                    .push((stream.subscribe(), context.waker().clone()));
                                break Poll::Pending;
                            }
                            Ok(count) => {
                                if offset == chunk.len() {
                                    if flushing {
                                        break Poll::Ready(Ok(()));
                                    } else {
                                        stream.flush().expect("stream should be flushable");
                                        flushing = true;
                                    }
                                } else {
                                    let count =
                                        usize::try_from(count).unwrap().min(chunk.len() - offset);

                                    match stream.write(&chunk[offset..][..count]) {
                                        Ok(()) => {
                                            offset += count;
                                        }
                                        Err(_) => break Poll::Ready(Err(anyhow!("I/O error"))),
                                    }
                                }
                            }
                            Err(_) => break Poll::Ready(Err(anyhow!("I/O error"))),
                        }
                    }
                }
            })
        })
    }

    pub fn outgoing_request_send(
        request: OutgoingRequest,
    ) -> impl Future<Output = Result<IncomingResponse, types::ErrorCode>> {
        future::poll_fn({
            let response = outgoing_handler::handle(request, None);

            move |context| match &response {
                Ok(response) => {
                    if let Some(response) = response.get() {
                        Poll::Ready(response.unwrap())
                    } else {
                        WAKERS
                            .lock()
                            .unwrap()
                            .push((response.subscribe(), context.waker().clone()));
                        Poll::Pending
                    }
                }
                Err(error) => Poll::Ready(Err(error.clone())),
            }
        })
    }

    pub fn incoming_body(body: IncomingBody) -> impl Stream<Item = Result<Vec<u8>>> {
        enum Inner {
            Stream {
                stream: InputStream,
                body: IncomingBody,
            },
            Trailers(FutureTrailers),
            Closed,
        }

        struct Incoming(Inner);

        impl Drop for Incoming {
            fn drop(&mut self) {
                match mem::replace(&mut self.0, Inner::Closed) {
                    Inner::Stream { stream, body } => {
                        drop(stream);
                        IncomingBody::finish(body);
                    }
                    Inner::Trailers(_) | Inner::Closed => {}
                }
            }
        }

        stream::poll_fn({
            let stream = body.stream().expect("response body should be readable");
            let mut incoming = Incoming(Inner::Stream { stream, body });

            move |context| loop {
                match &incoming.0 {
                    Inner::Stream { stream, .. } => match stream.read(READ_SIZE) {
                        Ok(buffer) => {
                            return if buffer.is_empty() {
                                WAKERS
                                    .lock()
                                    .unwrap()
                                    .push((stream.subscribe(), context.waker().clone()));
                                Poll::Pending
                            } else {
                                Poll::Ready(Some(Ok(buffer)))
                            };
                        }
                        Err(StreamError::Closed) => {
                            let Inner::Stream { stream, body } =
                                mem::replace(&mut incoming.0, Inner::Closed)
                            else {
                                unreachable!();
                            };
                            drop(stream);
                            incoming.0 = Inner::Trailers(IncomingBody::finish(body));
                        }
                        Err(StreamError::LastOperationFailed(error)) => {
                            return Poll::Ready(Some(Err(anyhow!("{}", error.to_debug_string()))));
                        }
                    },

                    Inner::Trailers(trailers) => match trailers.get() {
                        Some(Ok(trailers)) => {
                            incoming.0 = Inner::Closed;
                            match trailers {
                                Ok(Some(_)) => {}
                                Ok(None) => {}
                                Err(error) => {
                                    return Poll::Ready(Some(Err(anyhow!("{error:?}"))));
                                }
                            }
                        }
                        Some(Err(_)) => unreachable!(),
                        None => {
                            WAKERS
                                .lock()
                                .unwrap()
                                .push((trailers.subscribe(), context.waker().clone()));
                            return Poll::Pending;
                        }
                    },

                    Inner::Closed => {
                        return Poll::Ready(None);
                    }
                }
            }
        })
    }
}
