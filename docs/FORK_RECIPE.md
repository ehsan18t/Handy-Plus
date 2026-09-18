# Adding a feature to Handy-Plus

Read [FORK.md](../FORK.md) first. It is the map: what the fork adds today and which decisions were taken on purpose. This file is the method: how to add the next thing without making the next upstream sync worse.

## The one number that matters

Maintaining a fork costs you nothing for the code you write in your own files, and costs you on every single upstream release for the code you write in theirs. A file upstream never opens is free no matter how large. Ten lines in a file upstream rewrites is not.

That is the whole design. Everything below follows from it.

## The two rules

**1. Fork code lives under `fork/`.** That is `src-tauri/src/fork/` on the backend and `src/fork/` on the frontend. Upstream has no file at either path, so nothing you put there can ever conflict.

**2. Upstream files reach the fork through one door.** `fork::hooks` on the backend, `@/fork` on the frontend. An upstream file gets a one-line call and nothing else: no imports of fork internals, no inlined blocks, no reaching past the seam for one convenient type.

Rule 2 is the one that decays, so it is checked. `fork-guard.mjs verify` fails if any upstream file names a fork path other than `fork::hooks`.

Why one line and not five: when upstream rewrites the function around your call, a one-line conflict is mechanical (keep their code, keep your line). A twenty-line conflict is a judgement call, and it arrives six months later when you no longer remember what the block was for. Judgement calls under time pressure are where a fork quietly loses a feature.

## The exception: finishing something upstream already half has

The two rules assume the fork is adding what upstream does not have. Occasionally it is finishing what upstream already half has, using only upstream's data and upstream's concepts. The History page's Post Process tab is the first case: `post_processed_text` has been a column on `transcription_history` since an early migration, and the page simply never rendered it.

For that case both rules invert. Write the change where upstream would have written it, inline, and accept the footprint.

The reason is drift, not conflict. Fork-owning a copy of an upstream component buys zero conflicts and zero of upstream's fixes to it, and nothing tells you which fixes you missed. `HistorySettings.tsx` took 13 upstream commits in the last 300. A conflict is loud. Drift is silent, and silent is worse.

Three things have to hold before taking the exception:

1. The feature uses no fork type, no fork module and no fork setting. The moment it needs one, it is fork code and rule 1 applies again.
2. The hunks are insertions rather than reflows, so upstream's own edits around them still merge.
3. It is one self-contained commit, so it cherry-picks to an upstream PR without surgery.

If a command lands in an upstream module this way, give it its own invariant in `fork-guard.mjs` and a matching `guard-test.mjs` mutation. `EXPECTED_COMMANDS` matches `fork::hooks::` names only, so nothing else would notice a merge in `lib.rs` dropping the registration.

Strings still go in `fork.json`, never `translation.json`. `scripts/check-translations.ts` validates only `translation.json`, so an English-only key there fails the gate for all 24 other locales. Moving them is part of preparing the PR, not part of writing the feature.

## Where things go

| What                            | Where                                                                   |
| ------------------------------- | ----------------------------------------------------------------------- |
| Backend feature code            | `src-tauri/src/fork/<feature>/`                                         |
| The call upstream makes into it | one function in `src-tauri/src/fork/hooks.rs`                           |
| Tauri commands                  | `src-tauri/src/fork/<feature>/commands.rs`, re-exported from `hooks.rs` |
| Frontend feature code           | `src/fork/<feature>/`                                                   |
| Pages upstream renders          | exported from `src/fork/index.ts`                                       |
| User-visible strings            | `src/i18n/locales/en/fork.json`                                         |
| Fork settings                   | a field on `ForkSettings`, not a new field on `AppSettings`             |

Nothing in that table is a new upstream file. That is the point: a second feature should touch the same handful of upstream files the first one already touches, or none at all.

## Adding a hook

1. Write the behaviour in `fork/<feature>/`.
2. Add one `pub fn` to `fork/hooks.rs` that calls it. Put the reasoning in its doc comment, not at the call site: the upstream file should stay as close to upstream as it can.
3. Call it from the upstream file, on one line, with a `// Fork: see fork::hooks::<name>.` comment above it.
4. If the seam needs to call back into upstream code that is private, widen it to `pub(crate)`. A one-word visibility change is the cheapest hook shape there is, and it is much better than copying the function into the fork where it will drift.

Then add two invariants to `fork-guard.mjs`, not one:

- that the behaviour is still in `hooks.rs`
- that the upstream file still **calls** it

The second is not optional. Every check that reads `hooks.rs` passes happily while the upstream file has stopped calling into the fork entirely, and a hook nothing calls is still a perfectly correct hook.

## Adding a command

1. Write it in `fork/<feature>/commands.rs` with `#[tauri::command]` and `#[specta::specta]`.
2. `fork/hooks.rs` already does `pub use cloud::commands::*;`. A glob, because `collect_commands!` needs the hidden macros `#[tauri::command]` generates and a named re-export drops them.
3. Add one line to the `collect_commands!` list in `lib.rs`, spelled `fork::hooks::<name>`.
4. Add the name to `EXPECTED_COMMANDS` in `fork-guard.mjs`, or a merge can drop the registration and the UI fails at runtime only.
5. Regenerate bindings (below).

`guard-test.mjs` hardcodes the check name `all N fork commands are registered`. Growing the list renames that check and the self-test exits 2 until you update it. It fails loudly, which is the intended behaviour, but it does mean two files to edit.

## Adding settings

Put the field on `ForkSettings` in fork-owned code. `AppSettings` then never grows again, whatever you add.

Three `cloud_*` fields sit directly on `AppSettings` because they predate this and moving them needs a migration that touches stored secrets. Do not copy that shape.

Static per-provider or per-feature metadata is **not** settings. It belongs in code, keyed by id, the way `fork/cloud/providers.rs` does it. Persisting something that code re-derives on every load buys nothing and costs a conflict every time upstream edits the struct you hung it on.

## Adding strings

Everything user-visible goes in `src/i18n/locales/en/fork.json`, never in upstream's `translation.json`. Upstream ships two dozen translation files and the fork edits none of them, so a sync can never conflict on one. The cost is that fork strings fall back to English in other locales, which is a trade already made and recorded in FORK.md.

## After any backend change

```bash
cd src-tauri
cargo fmt
cargo check --lib
cargo run --bin handy -- --list-devices   # regenerates src/bindings.ts
cd ..
bunx tsc --noEmit
bun run lint
```

Never hand-edit `src/bindings.ts`. It is generated by tauri-specta during `run()` under `debug_assertions`, and `--list-devices` exits right after the export. A hand-merged copy compiles and is wrong.

## Before committing

```bash
node .claude/skills/sync-upstream/fork-guard.mjs verify      # all green
node .claude/skills/sync-upstream/guard-test.mjs             # all mutations caught, clean tree required
cargo test --lib
bun run check:translations
```

`verify` catches what compiles while being broken. `guard-test` catches the checker rotting into one that only ever reports success, which reads exactly like one that works.

Then update FORK.md's upstream-files table if you touched a new upstream file. `verify` fails if you did not, and it names the file. Keep that table honest: it is what the next sync reads to judge its own risk, and a map that quietly drops an entry is worse than no map.

## Once per clone

```bash
git config merge.ours.driver true
```

`.gitattributes` marks `bindings.ts` and `Cargo.lock` as `merge=ours` so git stops asking about files you regenerate anyway. Without this config the attribute is ignored.

## Things that are not worth doing

- **Copying an upstream function into fork code to avoid a `pub(crate)`.** The copy drifts, silently, and you find out when the two behave differently.
- **A parallel type that mirrors an upstream one.** You now maintain a mapping that breaks every time upstream adds a field.
- **Reaching past the seam for one type.** The next person copies it, and the surface starts growing again where nobody is looking.
- **A check without a matching `guard-test` mutation.** An unproven check is worse than no check, because it reports safety.
