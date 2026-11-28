# ADR-224: Model Judgment Verb

- **Status:** Proposed
- **Date:** 2026-11-27
- **Grounding commit:** `4a0af99babfdd3e8352692b2757cd00ccbc9ff39`
- **Relates to:** [ADR-181](ADR-181-exec-verb-sandboxed-run.md) (a run has no network, so this verb is the
  daemon's, never a run's)

It does not change the status of any other record.

## Context and scope

A caller that must judge many independent items, for example 60 candidate matches per cycle, judges each of them in
its own context today. No verb, on the store or on a computer, takes a prompt and returns a model answer. No source
file under `crates/*/src` calls a model, `agent.spawn` refuses every provider
(`khive-pack-agent/src/handlers.rs:109-119`), and a run on a computer cannot reach the network (ADR-181 Decision
step 3).

This record adds one read verb that sends one prompt to one configured model and returns the answer validated
against a schema the caller gives. It writes nothing to the store. It does not cover tool use, conversations,
streaming, spend accounting, or which callers of a hosted deployment may use it; the deployment's gate decides that.

## Decision

1. **A pack, `model`, with one verb, `model.judge`.** The pack registers its verb only when the configuration names
   at least one profile. With none it registers nothing, and it is not in the default pack set.
2. **Profiles.** Each `[model.profiles.<name>]` table has `endpoint`, an https URL whose host is fixed at load;
   `model`, the model id sent; `credential_env`, the name of the environment variable that holds the credential,
   optional for an endpoint that needs none; `max_output_tokens`, 1 to 32,768; `timeout_s`, 1 to 600; and
   `concurrency`, 1 to 64. The request shape is the chat-completions form with a JSON schema response format, which
   hosted gateways and local servers both serve. Refused at load, naming the profile and the key: a name outside
   `[a-z0-9-]`, an `http` endpoint whose host is not a loopback address, an endpoint carrying user information or a
   query, an empty `model`, and any number out of its range.
3. **The call.** `model.judge(prompt, schema, profile)`. Refused before any call, each with its code: a prompt that
   is empty or only whitespace (`empty_prompt`); a prompt over 200,000 bytes (`prompt_too_large`); a profile not
   configured (`unknown_profile`, listing the configured names); a schema that does not compile as a JSON Schema
   (`schema_invalid`, with the compiler's first error); and a credential variable that is unset or empty
   (`credential_missing`, naming the variable). The model receives the prompt as the one user message and the schema
   as the response format. It receives nothing else: no text from the store, no earlier calls, no tools.
4. **Egress.** Each profile has one client that connects only to its endpoint's host, follows no redirect and reads no
   proxy setting from the environment. A redirect is refused as `provider_refused` naming its status. The daemon
   reads the credential from its own environment at each call and sends it only in the request's authorization
   header. No reply, receipt, refusal or log line carries its value; a refusal names the variable only.
5. **The answer.** The model's text is parsed as JSON and validated against the schema. A valid answer returns
   `result`, the parsed value, with the receipt. Refused after the call, each with the receipt: text that is not JSON
   or fails the schema (`result_invalid`, with the validator's first error); a non-success status from the provider
   (`provider_refused`, with the status); and no answer within `timeout_s` (`timeout`). `result_invalid` and
   `provider_refused` carry the provider's text as `untrusted_text`, data for the caller and never an instruction to
   the daemon, cut at 16,384 bytes on a character boundary, with `untrusted_text_truncated` and the full length in
   bytes.
6. **The receipt rides the reply.** Every reply, refusals included, carries `receipt`: `profile`, `model_requested`,
   `model_served` (read from the provider's response, null when it names none), `called` (false for a refusal
   before the call), `started_at`, `elapsed_ms`, `prompt_digest` and `schema_digest` (BLAKE3 hex of the prompt bytes
   and of the schema's canonical JSON), `input_tokens` and `output_tokens` (null when the provider reports none),
   `provider_request_id` (null when absent) and `outcome`. Nothing is stored. A caller that needs the receipt later
   stores it with its own result.
7. **Concurrency.** Calls on one profile beyond its `concurrency` wait in arrival order. A call that waits longer than
   `timeout_s` is refused `timeout` with `called: false`.
8. **Admission.** `model.judge` writes nothing to the store, so it joins `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`
   and ADR-103 Amendment 3's list.

## Acceptance

The arms run against a fixture provider, a local server the test starts, which records each request it receives.

1. **Batch.** One request of 60 independent `model.judge` ops on a profile with `concurrency` 8 completes; each
   result validates against the schema, each receipt names the profile, and the fixture never holds more than 8
   requests at once.
2. **Invalid answers.** Text that is not JSON and JSON that fails the schema are each refused `result_invalid` with
   the first error, `untrusted_text` and a receipt with `called: true`. A 20,000-byte answer is cut at 16,384 bytes
   with the flag and the full length.
3. **Empty prompt.** An empty prompt and a whitespace prompt are refused `empty_prompt`, and the fixture's request
   count is unchanged.
4. **Before the call.** An unknown profile (listing the names), a schema that does not compile and a prompt over the
   limit are refused with their codes, and the fixture's request count is unchanged.
5. **Credential.** With the variable unset the call is refused `credential_missing` naming it. With it set, the
   fixture receives it in the authorization header, and the test finds its value in no reply, receipt, refusal or
   captured log line.
6. **Egress.** A redirect from the fixture is refused `provider_refused` naming its status, and the redirect target
   receives nothing. An `http` endpoint on a host that is not loopback, an endpoint with user information and one
   with a query are each refused at load.
7. **Nothing written.** The store's record and event counts are unchanged across arm 1.
8. **Not loaded.** With no profile configured, `model.judge` is not in the verb list.
9. **Served model.** The fixture names a model different from the one requested; the receipt carries both.
