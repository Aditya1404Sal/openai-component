# OpenAI Component

This is a Rust WebAssembly component that provides the core OpenAI API integration functionality. It's designed to be composed with an HTTP proxy component that handles routing and HTTP protocol concerns.

## Features

- Streams OpenAI API responses in real-time
- Forwards prompts to OpenAI's `/v1/responses` endpoint with `gpt-4.1` model
- Returns responses as `text/event-stream` for efficient streaming
- Exports a `stream-handle` WIT interface for composition with other components
- Requires `OPENAI_API_KEY` environment variable for authentication

## Architecture

This component is designed to be **composed** with an HTTP proxy component. It provides the low-level OpenAI API integration while the HTTP proxy handles:
- HTTP request routing (e.g., `POST /openai-proxy`)
- WASI HTTP protocol handling
- Request/response lifecycle management

The composition is done using the `wac` tool:

```bash
# From the http-proxy directory
wac plug ./build/http_proxy.wasm --plug ../openai-component/build/openai_component.wasm -o final.wasm
```

## Prerequisites

- `cargo` 1.82
- [`wash`](https://wasmcloud.com/docs/installation) 0.36.1
- `wasmtime` >=25.0.0 (if running with wasmtime)

## Building

```bash
wash build
```

## Usage

This component **cannot** be run standalone. It must be composed with an HTTP proxy component that provides the HTTP interface layer. See the composition example in the Architecture section above.

Once composed, the final component can be run with wasmtime or deployed to wasmCloud with the `OPENAI_API_KEY` environment variable configured.

## WIT Interface

The component exports a `stream-handle` function defined in the `wasmcloud:ai/streaming-handler` interface:

```wit
stream-handle: func(prompt: string, response: response-outparam);
```

This function:
- Accepts a text prompt string
- Takes a response output parameter for streaming
- Streams OpenAI API response chunks back through the response parameter

## How It Works

1. The component receives a text prompt via the `stream-handle` function
2. It constructs an HTTP POST request to `https://api.openai.com/v1/responses`
3. The request includes:
   - Model: `gpt-4.1`
   - The user's prompt as input
   - Streaming enabled (`"stream": true`)
4. The component streams the response chunks back to the caller as `text/event-stream`
5. All chunks are logged with detailed metrics for debugging
