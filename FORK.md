# Handy-Plus

A fork of [cjpais/Handy](https://github.com/cjpais/Handy) that adds two things:
credential rotation across API keys, and cloud speech-to-text with local
fallback.

This document is the map. It says what the fork adds, where it lives, which
upstream files it touches, and which decisions were made deliberately so they are
not undone by accident. It deliberately carries no code and no line numbers,
because both drift on every upstream sync. Read it to orient, then explore.

## What the fork adds

**Credential rotation.** You hold several API keys and Handy spreads requests
across them so no single key is exhausted. Each key in a rotation carries its own
model, so keys from different providers can share one list. The same key may
appear more than once with different models, because providers meter quota per
model, so those are independent buckets.

When a request fails, what happens depends on why. A rate limit that says when to
retry pauses that key for exactly that long and costs it nothing. A rejected key
is marked invalid and skipped entirely. A failure that never reached the provider,
such as being offline or a misconfiguration caught before sending, is not held
against the key at all. Everything else counts as a strike, and enough strikes
inside a window bench the key for a cooldown.

Selection is either round robin, which follows the order you arranged, or least
recently used, which spreads load and survives a restart.

**Cloud speech-to-text.** Recordings can be transcribed by a provider instead of
the local model. It is a toggle rather than an entry in the model selector.
Exhausting the rotation falls back to the local model, or surfaces the failure if
you turn that off. Live transcription preview is suppressed while it is on,
because a remote provider returns the whole transcript at once.

**Post-processing through the rotation.** Cleanup can run through the pool
instead of a single key, with a per-entry instruction template. An empty rotation
means upstream's original single-key path runs, unchanged.

## How it fits together

The fork's code lives in two directories: `src-tauri/src/cloud/` on the backend
and `src/components/settings/providers/` on the frontend, with its strings in a
separate `fork.json` per locale. None of those existed upstream, so a merge can
never conflict on them. Keeping fork behaviour inside them is the single most
useful habit for this project.

The backend splits along one line. The pool itself is free of Tauri and of
settings persistence: it resolves configuration into candidates, applies
eligibility rules, orders them by policy, and drives the attempt loop, taking the
actual network call as a closure. That is why its rules are unit-testable. The
glue that needs an app handle, such as emitting events and writing validity back
to settings, is separated out.

Configuration lives in the settings file. Rotation state does not. Last-used
timestamps, strike counts and cooldown deadlines mutate on every request, while
every settings write reserializes the whole object, and the settings loader
silently replaces any field it cannot parse with a default. So that state gets
its own SQLite database with its own migration chain, keyed by credential,
capability and model.

Cloud speech hooks in above the transcription manager rather than inside it. The
manager dispatches on an engine enum matched at roughly fifteen sites, most
assuming an in-process engine with a load lifecycle, a GPU device and an unload
timer, none of which a remote endpoint has. One branch at the async call site
above costs nothing and leaves the local path untouched.

Fork strings live in their own i18n namespace, so upstream's two dozen
translation files are never edited.

## Upstream files the fork touches

This is the entire conflict surface. Everything else is additive and lives in
fork-owned files.

**Generated. Never merge these by hand, regenerate them.**

| File | Why the fork touches it |
| --- | --- |
| `src/bindings.ts` | Tauri type bindings, produced by running the app |
| `src-tauri/Cargo.lock` | dependency graph |

**Hand-written. These carry the hooks.**

| File | What the fork holds there |
| --- | --- |
| `src-tauri/src/settings.rs` | Provider capabilities, speech endpoints, whether a provider needs a key; a pass that stamps this onto upstream's provider list by id; the fork's own settings fields |
| `src-tauri/src/llm_client.rs` | Typed errors carrying status and retry-after on five call paths, plus the redaction helper the pool needs to classify failures |
| `src-tauri/src/actions.rs` | Four hooks: cloud speech above the transcription manager, deferring the local model load when a provider will serve, dispatching post-processing, and suppressing streaming |
| `src-tauri/src/lib.rs` | Module declaration, pool construction at startup, command and event registration |
| `src-tauri/src/shortcut/mod.rs` | Clearing rotation entries that named an instruction template when it is deleted |
| `src-tauri/src/managers/transcription.rs` | Exposing the local text cleanup so cloud transcripts get the same treatment |
| `src-tauri/Cargo.toml` | Multipart upload support and a shared byte buffer type |
| `src-tauri/tauri.conf.json` | Updater repointed at this fork. Must stay diverged, and upstream's endpoint must not survive beside it: tauri tries them in order and both repos share a signing key |
| `src/App.tsx` | Listener for the degradation event |
| `src/components/Sidebar.tsx` | Two new pages, and the post-processing page swapped for the fork's |
| `src/components/settings/index.ts` | Exports for the fork's pages |
| `src/components/ui/Dropdown.tsx` | Menu rendered through a portal and positioned against the viewport, so it stops clipping inside scroll containers |
| `src/components/ui/Badge.tsx` | A danger variant, so a rejected key does not render in the brand colour |
| `src/i18n/index.ts` | Registers the fork's string namespace |
| `.gitignore` | Re-includes the skills directory upstream excludes |

## Decisions taken deliberately

Do not "fix" these without reading why they are what they are.

**Secrets live in the settings file, not an OS keychain.** This matches
upstream's existing posture for its own API key. Doing better means new
cross-platform dependencies and a split where upstream's key sits in one place
and the fork's in another. Deferred knowingly.

**Rotation state is a separate SQLite file** rather than part of settings or part
of upstream's history database, so per-request writes stay cheap and the fork's
migration chain can never collide with upstream's.

**Cloud speech is a toggle, not a model-selector entry.** The alternative means
touching every site that matches on the engine enum to say "not applicable". The
trade is that cloud models do not appear in the Models tab.

**Cloud speech and live streaming are mutually exclusive.** Not cosmetic: a
streaming engine finalizes first and its text would be used before the cloud path
ever ran.

**Always-on microphone stays fully local.** No ambient audio leaves the machine
and no unbounded quota burn.

**An empty rotation runs upstream's single-key post-processing.** This is what
stops an upgrade from silently losing the feature for someone who configured it
before the fork existed. Provider configuration for that path lives on the API
Keys page, not duplicated onto the post-processing page.

**Upstream's hardcoded provider ids are left alone.** Refactoring them is a
separate, optional piece of work and would enlarge the fork's diff for no
functional gain.

## Deliberately not built

- **Chunking oversized recordings.** Past the upload cap, speech falls back
  instead. Splitting audio cuts words at boundaries and multiplies requests per
  dictation, which interacts badly with per-request rotation accounting.
- **A fallback toggle for post-processing.** It has exactly one possible
  fallback, returning the transcript unmodified, so a switch would only choose
  between keeping and losing the dictation. Speech is different, which is why the
  toggle is live there.
- **Recording which credential served each request in history.** The history
  entry type is upstream's and is rendered by upstream's UI, so the change would
  ripple across three upstream surfaces. The status view and the log already
  cover the diagnostic need.

## Known gaps

- **Degradation notices render into the main window**, which is usually hidden
  during dictation. Inherited: upstream's own recording, paste and transcription
  error notices have the same limitation. Closing it properly means a tray
  notification or a surface on the overlay, which is a feature rather than a fix.
  Every degradation is always in the log.
- **A stalled provider still costs time.** Cloud speech has a ceiling over the
  whole rotation and a shorter one per request. Cancellation is honoured promptly,
  but a provider that accepts a connection and then goes quiet still burns its
  share of the budget before falling back.
- **Fork strings are English only.** They fall back to English in other locales
  rather than showing raw keys.

## Syncing with upstream

Use the `sync-upstream` skill in `.claude/skills/sync-upstream/`. It carries the
procedure and the per-file resolution rules.

Alongside it, `fork-guard.mjs` does the two things worth automating. It runs under
node or bun on any OS with no dependencies:

- `survey` reports what a sync would collide with, tagging each file as
  generated, hook-carrying, or additive
- `verify` checks the fork behaviours that compile perfectly while being broken,
  which no ordinary gate can see

`guard-test.mjs` beside it breaks each of those behaviours in turn and requires
the matching check to notice, so the checker cannot quietly rot into one that
only ever reports success.

One of those checks reads this document: if the fork starts editing an upstream
file that the table above does not list, it fails and names the file. **Keep that
table current.** It is what the next sync uses to judge its own risk, and a map
that quietly drops an entry is worse than no map.
