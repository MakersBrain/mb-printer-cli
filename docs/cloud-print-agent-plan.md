<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Cloud printing and outbound printer-agent plan

Status: proposed implementation contract

Date: 2026-08-30

Implementation evidence is tracked in
[`cloud-print-agent-status.md`](cloud-print-agent-status.md).

## 1. Decision

Keep the existing authenticated loopback HTTP API. Do not replace it with
gRPC.

Add an optional outbound connection from `mb-printer` to a cloud print service.
The native agent-to-cloud connection uses bidirectional gRPC over TLS on port
443. Browsers, Odoo, and other backend clients use the cloud service's HTTPS
JSON API; they do not connect to gRPC directly.

```text
Local printing

browser -> loopback HTTP API -> shared executor -> printer

Cloud printing

browser/Odoo -> cloud HTTPS API -> SQLite job
                                      |
                                      | outbound gRPC stream
                                      v
                              mb-printer agent -> shared executor -> printer
```

The agent always initiates the connection. Cloud printing requires no inbound
workshop port, VPN, or router configuration. The loopback service continues to
bind only to `127.0.0.1` and `::1`.

## 2. Outcomes

The first release must allow an authorized workshop user or backend to:

- enroll one `mb-printer` agent into one workshop;
- publish selected locally configured printer connections;
- see whether a published printer is online or unavailable;
- submit a canonical label document to one explicit printer;
- observe current job progress and request cancellation;
- revoke an agent remotely; and
- continue using local printing when the cloud is unavailable.

The cloud route must preserve the current local safety behavior:

- the cloud cannot construct a local transport;
- no job is automatically reassigned to another agent;
- the same job is deduplicated after reconnect or restart;
- a job is never automatically retried after a printer write may have occurred;
- cancellation before and after the first write remains distinguishable; and
- uncertain physical output is reported as `outcome-unknown`.

## 3. Non-goals

The first release does not include:

- gRPC-Web or browser-native gRPC;
- printer pools, load balancing, failover, or priority scheduling;
- one agent belonging to multiple workshops;
- arbitrary cloud-triggered discovery;
- cloud-provided serial paths, Bluetooth addresses, TCP destinations, file
  paths, USB selectors, or other transport configuration;
- synchronization of private local asset catalogues;
- object storage, payload compression negotiation, or chunked payload transfer;
- mutual-TLS device certificates or a private certificate authority;
- an append-only job-event system;
- exactly-once physical-printing claims; or
- indefinite document retention.

These features require evidence from the first deployment before being added.

## 4. Component ownership

### `mb-printer-cli`

Owns:

- the existing CLI and loopback API;
- cloud enrollment and agent configuration;
- the outbound gRPC client;
- published local-connection mappings;
- durable local cloud-job state;
- job deduplication; and
- all rendering, protocol planning, transport access, and physical execution.

### Cloud print service

Owns:

- the tenant HTTPS API;
- the agent gRPC endpoint;
- enrollment and revocable agent tokens;
- the printer registry and online status;
- durable cloud jobs in SQLite; and
- job delivery, progress, cancellation, and results.

Implement this as a standalone service and repository, `mb-print-cloud`. It
must not link to `mb-control-plane`, Odoo, or another product backend. Internally
it uses a generic `tenant_id`; a MakersBrain workshop is one tenant, while
another product may map its own account or organization to the same contract.

The service owns its SQLite schema and migrations, HTTPS API, gRPC broker, and
deployment image. Product backends are ordinary HTTPS clients. They do not
receive database access or agent credentials.

The first deployment mode is deliberately small: one binary, one owner-only
configuration file, and one SQLite database file. The configuration declares
one tenant and static API bearer credentials with `print` and/or
`manage-printers` permissions. Credentials may be stored as hashes; an `init`
command generates the config and prints each plaintext credential once. Do not
implement user accounts, memberships, sessions, or an identity provider.

Configurable OIDC JWT validation and multiple tenants may be added as adapters
later without changing the print/job API. The agent enrollment/token flow is
always owned by the print service.

Representative server setup:

```text
mb-print-cloud init --config /etc/mb-print-cloud.toml
mb-print-cloud serve --config /etc/mb-print-cloud.toml
```

The config contains the public URL, listen address, SQLite path, tenant ID and
display name, hashed API credentials, and request limits. The v1 process listens
on loopback behind a TLS reverse proxy; keeping certificate renewal out of the
application makes the single-binary service easier to operate. Production
agent URLs must always be HTTPS; plain HTTP is allowed only on loopback for
tests and local development.

### `mb-label-editor` and `mb-odoo-addons`

May call the cloud HTTPS API after the agent path is proven. They never receive
agent credentials and never select local transports.

## 5. Shared local executor

Refactor the existing print-job runner behind one internal `JobExecutor`
interface used by:

- direct CLI printing;
- the loopback HTTP API; and
- cloud jobs.

The executor remains the only component allowed to:

- validate document, printer, media, and print options;
- create the protocol execution plan;
- open a native transport;
- send bytes to a printer;
- report when the first write may have been accepted;
- classify cancellation and ambiguous outcomes; and
- produce normalized progress and terminal results.

The refactor must not change the existing loopback OpenAPI contract or current
CLI behavior.

Serialize execution per locally configured connection. Even if the cloud sends
duplicate or conflicting work, only one job can use a connection at a time.

## 6. Enrollment and authentication

Use a simple token flow over server-authenticated TLS.

1. A workshop administrator creates a one-time enrollment code through the
   cloud HTTPS API.
2. The service stores only the code's hash, workshop, creator, and expiry.
3. The user enters the code into `mb-printer cloud enroll`.
4. The service consumes the code and returns a random agent ID and bearer token.
5. The agent stores the token in an owner-only local file.
6. The service stores only a hash of the agent token.
7. The agent uses the token as gRPC authorization metadata over TLS.

Enrollment codes must be single-use, contain at least 128 bits of entropy, and
expire within 10 minutes. Agent tokens must contain at least 256 bits of
entropy. Rate-limit enrollment attempts.

The local token file is written atomically and must not be readable by group or
other users. Enrollment secrets and agent tokens must not be logged or accepted
as ordinary command-line arguments that appear in process listings. Read them
interactively or through stdin/an exact protected file.

An administrator can revoke an agent token. Revocation prevents new
connections, closes the active stream where practical, disables the agent's
published printers, and prevents new jobs from being submitted to them.

Token rotation may be added as a small authenticated endpoint. Private keys,
CSRs, certificate renewal, and mTLS are deferred.

## 7. Published printers

Cloud users print only to connections deliberately published by the local
administrator.

Representative commands:

```text
mb-printer cloud enroll --server https://print.example
mb-printer cloud publish --connection desk --name "Packing desk"
mb-printer cloud unpublish PRINTER_ID
mb-printer cloud status
mb-printer cloud connect
```

Publishing records:

- cloud printer ID;
- agent ID;
- local saved-connection ID;
- display name;
- printer model and safe capabilities; and
- enabled/disabled state.

The local connection definition remains on the agent. The cloud never receives
the serial path, Bluetooth address, TCP destination, file path, USB selector,
or other connection secrets.

A cloud job contains a cloud printer ID, not a `transport` object. The agent
maps that printer ID to its saved local connection and rejects unknown,
disabled, or mismatched mappings before opening a transport.

If a local connection is materially replaced, unpublish and republish it with
a new cloud printer ID. Do not add publication generations until an actual
workflow requires in-place replacement.

## 8. Minimal gRPC contract

Use a versioned Protobuf package and `tonic`/`prost` in Rust:

```proto
package makersbrain.print.agent.v1;

service PrinterAgentService {
  rpc Session(stream AgentMessage) returns (stream BrokerMessage);
}

message AgentMessage {
  oneof payload {
    AgentHello hello = 1;
    Heartbeat heartbeat = 2;
    PrinterStatus printer_status = 3;
    JobReceived job_received = 4;
    JobProgress job_progress = 5;
    JobResult job_result = 6;
  }
}

message BrokerMessage {
  oneof payload {
    BrokerHello hello = 1;
    PrintJob print_job = 2;
    CancelJob cancel_job = 3;
  }
}
```

The exact message fields belong in the `.proto`, but v1 needs only:

- agent and protocol/software version in `AgentHello`;
- published-printer status and current job IDs in heartbeat/status messages;
- job ID, printer ID, canonical request bytes, and SHA-256 digest in
  `PrintJob`;
- job ID and persisted digest in `JobReceived`;
- the current local job view in `JobProgress`; and
- the existing normalized terminal outcome in `JobResult`.

Do not add generic policy updates, command envelopes, message cursors,
application-level sequence numbers, delivery attempts, or a separate
offer/start handshake in v1.

Protobuf rules:

- never reuse a field number;
- reserve removed field numbers and names;
- add only fields with safe defaults;
- reject unsupported protocol versions clearly; and
- keep the previous protocol compatible during one normal agent upgrade
  window after the first production release.

## 9. Job delivery and deduplication

Each job has one immutable cloud job ID and one immutable target agent/printer.
The cloud does not reassign a job to a different agent automatically.

Delivery works as follows:

1. The cloud validates and stores the complete job in SQLite.
2. When the target agent is connected, the cloud sends `PrintJob`.
3. The agent validates the size and SHA-256 digest.
4. The agent validates the document and options with the authoritative SDK.
5. The agent persists `(job_id, digest, printer_id, request, local_state)`
   before execution.
6. The agent sends `JobReceived` and queues the persisted job locally.
7. The shared executor runs it and reports progress/result.
8. The cloud stores the latest progress and terminal outcome.

On reconnect, the cloud may resend any non-terminal job assigned to that
agent. The agent handles it by job ID:

- same job ID and same digest: return the existing local state; do not create a
  second execution;
- same job ID and different digest: reject it as a conflict and do not print;
- locally terminal job: resend the stored terminal result; and
- locally non-terminal job: report its current state and continue only
  according to the local executor's persisted state.

The agent must persist enough state to distinguish:

- received but not started;
- running before the first possible printer write;
- running after a write may have occurred; and
- terminal outcome.

If the agent restarts after a write may have occurred and cannot prove the
result, it reports `outcome-unknown`. It does not start the job again.

This conservative ownership model avoids delivery attempts, leases, fencing
tokens, automatic failover, and a distributed two-phase start protocol.

## 10. Job states

Use the existing local terminal outcomes. The cloud job state is a projection
of the latest durable cloud and agent information:

```text
queued
  |-> cancelled-before-send
  `-> delivered
        |-> cancelled-before-send
        `-> running
              |-> completed
              |-> failed
              |-> cancelled-before-send
              |-> cancelled-partial
              `-> outcome-unknown
```

Additional rules:

- `queued` means the cloud has stored the job but the agent has not confirmed
  durable receipt.
- `delivered` means the agent has persisted the exact job.
- A disconnected agent does not cause reassignment or automatic failure.
- Cancellation while `queued` prevents delivery.
- Cancellation after delivery is cooperative.
- Once a printer write may have occurred, that fact is monotonic.
- Terminal outcomes are immutable.
- A reprint is a new job with a new ID and an optional reference to the old job.

The API and UI must not claim exactly-once printing. `outcome-unknown` requires
the user to inspect the physical printer before creating a separate reprint.

## 11. SQLite data model

Start with four logical tables. All tenant-owned rows include `tenant_id` and
all repository operations require an authenticated tenant scope. `tenant_id`
is an opaque UUID; the service does not need to know whether it represents a
workshop, organization, store, or another host-project account. SQLite foreign
keys and WAL mode are enabled at startup. The service runs as a single process;
multiple broker replicas and a PostgreSQL adapter are deferred until needed.

### `printer_agents`

- ID and tenant ID;
- display name and lifecycle state (`active` or `revoked`);
- software/protocol version;
- created, last-connected, last-heartbeat, and revoked timestamps; and
- safe last-disconnect/error code.

### `agent_tokens`

- agent ID and token hash;
- created, last-used, and revoked timestamps; and
- enrollment/audit reference.

Enrollment rows may be stored here or in a small separate table if atomic
single-use consumption is clearer.

### `printers`

- ID, tenant ID, and agent ID;
- display name, model, and safe capability projection;
- enabled and online status; and
- created, updated, and last-seen timestamps.

### `print_jobs`

- ID, tenant ID, submitting subject, and source;
- immutable agent and printer IDs;
- canonical request bytes/JSON and SHA-256 digest;
- idempotency key and request digest;
- current state and normalized terminal outcome;
- current progress/action/byte counters;
- whether a printer write may have occurred;
- cancellation request timestamp/subject;
- safe error code; and
- created, delivered, started, terminal, and deletion timestamps.

Do not add attempt, lease, certificate, publication-generation, or append-only
event tables in v1.

## 12. Public cloud API

Provide a small HTTPS JSON API with an OpenAPI 3.1 contract:

```text
POST /v1/tenants/{tenant}/printer-enrollments
POST /v1/printer-enrollments/exchange
GET  /v1/tenants/{tenant}/printer-agents
POST /v1/tenants/{tenant}/printer-agents/{agent}/revoke
GET  /v1/tenants/{tenant}/printers
POST /v1/tenants/{tenant}/print-jobs
GET  /v1/tenants/{tenant}/print-jobs/{job}
POST /v1/tenants/{tenant}/print-jobs/{job}/cancel
```

Printer publication is driven by the authenticated agent stream. Renaming or
disabling a printer may use an administration endpoint if the first UI needs
it; it is not required for the capture pilot.

Start with two permissions:

- `print`: list printers, submit jobs, read permitted jobs, and request
  cancellation;
- `manage-printers`: create enrollment codes and revoke agents.

Submission requires:

- one enabled cloud printer ID;
- one bounded canonical SDK v4 document;
- explicit print options; and
- an `Idempotency-Key`.

Idempotency is scoped to the authenticated tenant and subject. Exact replay
returns the original job; reuse with different request bytes returns HTTP 409.

Return HTTP 202 after durable storage. The job may remain queued while the
agent is offline.

The first client can poll `GET /print-jobs/{job}`. Add SSE only when the editor
integration needs live updates; SSE can initially publish the latest job row
without introducing an append-only event store or resumable cursor.

## 13. Payload limits and retention

Store the bounded canonical request directly in SQLite for v1. Send it in
the `PrintJob` gRPC message. Do not introduce object storage or compression
until measured job sizes or database volume justify them.

Both cloud and agent enforce:

- maximum encoded request size;
- maximum decoded document/resource size;
- existing copies, density, rotation, fitting, and payload-limit bounds;
- SHA-256 verification; and
- strict canonical document validation.

Delete document content after a short configured retention period, initially
no more than seven days. Keep only the metadata necessary for job history and
audit. Ambiguous jobs may retain their payload for the same bounded support
window but not indefinitely.

Documents may contain personal data. Never write document contents, barcode
values, embedded resources, enrollment codes, or agent tokens to logs, metrics,
traces, panic reports, or error responses.

## 14. Reconnection, availability, and backpressure

The agent reconnects with bounded exponential backoff and jitter. Heartbeats
keep printer availability current and include the IDs/states of local
non-terminal cloud jobs.

On each connection:

1. authenticate the agent token;
2. exchange supported protocol/software versions;
3. publish the current printer list/status;
4. compare non-terminal job IDs and states; and
5. resend missing jobs or terminal results by immutable job ID.

Default to one active job per local connection. The broker sends at most one
unacknowledged job at a time per agent during v1. This is sufficient for the
pilot and prevents an offline agent from receiving an unbounded burst after
reconnect.

The production reverse proxy must support HTTP/2 gRPC and have an idle timeout
compatible with the selected heartbeat. Test the deployed proxy rather than
assuming local behavior represents production.

## 15. Security requirements

Before physical printing is enabled, test at least:

- cross-workshop access to agents, printers, jobs, and enrollment codes;
- expired, consumed, brute-forced, and replayed enrollment codes;
- revoked, unknown, and malformed agent tokens;
- duplicate jobs with matching and conflicting digests;
- attempted cloud transport injection;
- oversized requests and resource expansion;
- agent and cloud crashes before and after the first printer write;
- disconnect during progress/result reporting;
- cancellation before and after the first printer write; and
- logs and traces for leaked credentials or representative document data.

Use TLS with normal hostname and certificate validation. Do not add an insecure
production mode. Derive the workshop from the authenticated agent or user
record, never from untrusted request metadata alone.

## 16. Minimal observability

Start with:

- connected-agent and online-printer gauges;
- queued, running, completed, failed, and ambiguous job counts;
- job queue and execution duration;
- authentication, payload-validation, and digest failures; and
- structured connection/job state-transition logs using safe IDs and codes.

Do not use workshop, agent, printer, job, document, or customer identifiers as
unbounded metric labels. Full cross-service tracing and detailed event history
can be added after the pilot if operations need them.

Audit enrollment creation/use, agent revocation, job submission/cancellation,
and explicit reprints. Ordinary progress updates do not need security-audit
entries.

## 17. Implementation milestones

### Milestone 1: shared executor and minimal contracts

- Extract `JobExecutor` without changing loopback behavior.
- Define the small v1 Protobuf and cloud OpenAPI contracts.
- Add local cloud configuration and durable job records.
- Add contract and regression tests.

Exit: existing CLI/loopback tests pass unchanged and cloud support remains
disabled by default.

### Milestone 2: enrollment, connection, and durable jobs

- Implement token enrollment, authentication, revocation, and publication.
- Implement the outbound gRPC stream and reconnect behavior.
- Add the four-table SQLite model and public job endpoints.
- Implement delivery, agent persistence, deduplication, progress, results, and
  cancellation.

Exit: a SQLite-backed cloud job survives cloud and agent restarts and is
visible again without duplicate local execution.

### Milestone 3: capture-only end-to-end qualification

- Publish a test-only capture connection.
- Test duplicate delivery and conflicting digests.
- Inject disconnects/crashes before receipt, before execution, before the first
  write, after the first write, and before terminal-result acknowledgement.
- Verify cancellation and `outcome-unknown` behavior.
- Verify two-workshop isolation and token revocation.

Exit: one cloud job produces at most one captured execution, and every
post-write uncertainty remains non-retryable.

### Milestone 4: guarded physical pilot and integrations

- Enable one qualified wired printer family for selected workshops.
- Add editor cloud-route selection and polling or simple SSE progress.
- Add Odoo submission only if required by the pilot.
- Write enrollment, revocation, proxy, unknown-outcome, and reprint runbooks.
- Collect physical evidence for disconnect and cancellation behavior.

Exit: users can distinguish local/direct/cloud routes, support can revoke an
agent and inspect ambiguous jobs, and no incident workflow requires database
editing or blind replay.

## 18. Acceptance criteria

The first cloud-printing release is ready when:

- the existing loopback API remains compatible;
- the agent connects outbound over TLS/443 with no inbound workshop port;
- cloud outages do not stop local printing;
- one-time enrollment and remote revocation work;
- workshop isolation fails closed;
- cloud jobs can reference only explicitly published local connections;
- the cloud cannot provide local transport details;
- jobs survive cloud and agent restarts;
- duplicate delivery by job ID and digest does not duplicate execution;
- conflicting duplicate payloads are rejected;
- the cloud never automatically reassigns a job;
- uncertainty after a possible printer write becomes `outcome-unknown`;
- request sizes and payload digests are checked before execution;
- credentials and document contents are absent from logs and metrics;
- capture-only crash/reconnect tests pass; and
- physical evidence exists for every printer family enabled in the pilot.

## 19. Deferred decisions

Reconsider these only with measured need:

- separate broker repository or deployment;
- mTLS agent certificates;
- object storage and streamed payloads;
- compression;
- printer pools and automatic assignment;
- attempt/lease/fencing records;
- publication generations;
- append-only job events and resumable SSE cursors;
- finer-grained permissions;
- per-platform credential stores;
- full distributed tracing; and
- multi-workshop agents.

Deferring them does not weaken the v1 safety model because each job has one
immutable target agent, the agent durably deduplicates it, and the cloud never
automatically reassigns it.
