These JSON fixtures were captured by `mcp_protocol.rs` against rmcp 1.5.0,
on Rust 1.88.0, before the dependency upgrade. They describe actual JSON-RPC
stdio traffic from the memory-mcp executable and HTTP requests received by
the in-process synthetic backend. No database or embedding service is used.

The tests compare JSON values, ignoring object-key order only. Tool discovery
is compared separately after sorting by tool name. The executable's version
must equal the build's package version and git hash before that exact field
is replaced with the fixture marker. Other identity fields, instructions,
schemas, descriptions, content, errors, IDs and HTTP payloads are compared
without normalization.

EOF before initialize exits unsuccessfully. EOF after initialization exits
successfully, including while a controlled HTTP call is still pending; that
pending call produces no response. The fixture releases its held backend
request and joins the HTTP server after the subprocess exits. Diagnostic
logging is read from stderr; every stdout line must parse as JSON-RPC.

Keep these baseline expectations unchanged when upgrading rmcp. A changed
fixture needs an independently justified contract change, not regeneration
to match the new SDK. Synthetic review responses check forwarding and MCP
formatting; the existing SQLx tests remain responsible for persisted review
verdict normalization and tags.
