---
name: sync-upstream
description: Use when pulling cjpais/Handy changes into the Handy-Plus fork - syncing with upstream, merging a new upstream release, resolving upstream conflicts, or updating the fork. Handles the merge, the generated files, the gates, and the invariants that compile while being broken. NOT for ordinary feature work on the fork.
---

# Syncing Handy-Plus with upstream

Upstream is `cjpais/Handy`. This fork adds credential rotation and cloud
speech-to-text. The job is to take upstream's work without losing either, and
the failure mode that matters is **a merge that compiles, passes every gate, and
is silently broken**. Most of this procedure exists to catch that.

**If you have not worked on this fork before, read `FORK.md` in the repo root
before starting.** It describes what the fork adds, which upstream files it
touches and why, and which choices were made on purpose. Resolving a conflict
without that context is how a deliberate decision gets quietly reverted.

## The shape of the risk

The fork is a large pile of new files plus edits to a much smaller set of
upstream ones. Git can never conflict on the new files. **The upstream edits are
the entire risk**, and two of them are generated, so a real sync comes down to a
dozen or so files. If a conflict appears anywhere else, something is wrong with
the sync rather than with the fork.

The exact counts are printed by the survey in step 1. They are not written down
here on purpose: every hard-coded total in this project has gone stale, so this
one is derived on the spot instead.

## Step 0: pre-flight, and stop if any of it fails

```
git remote get-url upstream          # MUST be cjpais/Handy, not the fork
git status --porcelain               # MUST be empty
git branch --show-current            # note it; this is what you are merging into
```

Plain `git`, no pipes. This fork is developed on Windows, where the shell is
PowerShell and `grep` does not exist. Every command in this file has to run
unchanged on any OS, which is the same reason the checker below is not a shell
script.

A dirty tree is a stop, not a warning. Merge conflicts on top of uncommitted work
are painful to unpick, and `git merge --abort` will not save you from it.

`upstream` has pointed at the fork itself before. When that happens `git fetch
upstream` succeeds, reports nothing, and the sync looks "already up to date"
rather than misconfigured. Verify the URL, never the exit code.

Then take rollback points and make git remember conflict resolutions:

```
git branch -f backup/pre-sync HEAD
git branch -f backup/pre-sync-main main
git config rerere.enabled true
git config rerere.autoupdate false
```

**Two refs, because step 2 moves `main` as well.** Fast-forwarding `main` is not
undone by `git merge --abort` or by resetting the fork branch, and `main`'s
pre-sync position is recorded nowhere else. Without the second ref, a retry after
an abort finds `main` already level with upstream, the survey reports zero
incoming commits, and step 1 tells you to suspect the remote. Report both refs to
the user.

**`autoupdate` is deliberately off.** With it on, a remembered resolution is
staged before anyone sees it: no conflict markers, status `M` rather than `UU`,
nothing inviting review, and a wrong one is then reproduced silently on every
retry while each attempt looks cleaner than the last. Off, rerere still replays
the resolution into the working tree but leaves the path unmerged, so you review
it and stage it yourself. Either way it is replayed, so **read every path rerere
touched**, especially the four hook files. Getting rid of a bad one is covered
under Aborting, and is harder than it looks.

## Step 1: survey before touching anything

```bash
git fetch upstream
node .claude/skills/sync-upstream/fork-guard.mjs survey
```

The survey reports the incoming commits and, more importantly, exactly which
files both sides touch, tagged as generated, hook-carrying, ours-wins, or
additive. That tagged list is the whole risk assessment.

**Show it to the user before merging.** If it names `actions.rs`, `settings.rs`,
`llm_client.rs` or `lib.rs`, say so explicitly: those four carry the hooks and
are where a plausible-looking resolution does real damage.

If the survey reports zero incoming commits, stop and say so. There is nothing to
do, and a sync that "succeeds" with no commits usually means a misconfigured
remote rather than being up to date.

## Step 2: merge

```bash
git checkout main && git merge --ff-only upstream/main
git checkout <fork-branch>
git merge main
```

If the fast-forward is refused, `main` has local commits on it. **Do not force
it.** Find out what they are (`git log upstream/main..main`) and tell the user;
fork work belongs on the fork branch, so anything sitting on `main` is either a
mistake worth understanding or something that needs moving first.

**Merge, never rebase.** The fork carries many commits, so a rebase replays each
one and can hand you the same `settings.rs` conflict a dozen times. A merge asks
once. Rebase only when preparing a patch to send upstream.

## Step 3: resolve

**Order matters here, and it is not the obvious one.** Hand-written files are
resolved *first*, because regenerating anything runs cargo, and cargo will not
parse a `Cargo.toml` with conflict markers in it. Getting this backwards ends the
sync at the first command with a raw TOML parse error and no hint that the fix is
to go back a step.

### 3a. Hand-written files: keep both sides

The fork's edits to upstream files are almost entirely **additive**. The default
resolution is "upstream's new code, plus the fork's additions", not a choice
between them. Two files are exceptions, marked below.

| File | What the fork holds | Resolution |
| --- | --- | --- |
| `settings.rs` | 4 fields on `PostProcessProvider`, its `Default` impl, `supports()`/`stt_url()`, `apply_fork_provider_metadata()`, 3 `cloud_*` fields | Keep both. If upstream added a field to `PostProcessProvider`, add it to our `Default` impl too or it stops compiling. |
| `llm_client.rs` | `ApiError` returns on 5 fns, `redact_and_truncate`, `report_reqwest_error` | Keep ours. Never accept a revert to `String` returns: the pool's failure classification depends on the typed error. |
| `actions.rs` | cloud STT hook, deferred model load, post-process dispatch, streaming suppression | Read the whole hunk. Order matters, see invariants below. |
| `lib.rs` | `pub mod cloud;`, pool setup, 7 commands, 1 event | Union of both command lists. |
| `Dropdown.tsx` | portal + viewport positioning | If upstream fixed clipping too, prefer theirs and drop ours. |
| `tauri.conf.json` | updater repointed at this fork | **Ours replaces theirs.** Not a union: tauri tries endpoints in order and both repos share a pubkey, so upstream's URL surviving alongside ours ships vanilla Handy over this build, signature intact. |
| `.gitignore` | `.claude/*` plus `!.claude/skills/`, in place of upstream's `.claude/` | **Ours replaces theirs.** A union keeps upstream's bare `.claude/` line, which re-excludes the directory and cancels the negation, because git cannot re-include a path whose parent is excluded. Everything under `.claude/skills/` then silently stops being addable. |
| `Sidebar.tsx`, `App.tsx`, `index.ts`, `Badge.tsx`, `i18n/index.ts`, `Cargo.toml`, `transcription.rs`, `shortcut/mod.rs` | small additive edits | Keep both. |

**If a conflict cannot be resolved as "both", stop and ask.** That means upstream
restructured something the fork hooks into, and guessing produces exactly the
silent breakage this procedure exists to prevent.

### 3b. Generated files: never merge by hand

```
git checkout --theirs src/bindings.ts src-tauri/Cargo.lock
git add src/bindings.ts src-tauri/Cargo.lock
```

During a merge `--theirs` is the branch being merged in, so on the fork branch
merging `main` it means upstream's copy. That is what you want here. The meaning
inverts during a rebase, which is one more reason this procedure merges.

**The `git add` is not optional.** `git checkout --theirs` writes the working tree
and leaves the index unmerged: it prints "Updated 1 path from the index" and exits
0 while `git status` still shows `UU`. On a path that did not conflict it updates
nothing and still exits 0, so its output tells you nothing either way.

### 3c. Regenerate, now that no file has markers in it

```
cd src-tauri
cargo check --lib                          # settles Cargo.lock
cargo run --bin handy -- --list-devices    # regenerates src/bindings.ts
cd ..
```

The specta export runs inside `run()` under `debug_assertions`, so the app has to
start; `--list-devices` exits right after, by which point the export has already
happened. Hand-merging `bindings.ts` produces a file that compiles and is wrong.

### 3d. Commit the merge

```
git add -A
git status --porcelain      # MUST show no U entries
git commit --no-edit
```

Nothing before this point staged the resolution, and every later step assumes it
is committed: step 5's map check diffs commits, not the working tree, so run
before this it judges the tree as it was *before* the sync and reports the merge
safe without having looked at it. If a gate below then forces a change, fold it in
with `git commit --amend --no-edit`.

## Step 4: gates

```
bun install                          # only if the merge touched package.json
cd src-tauri
cargo fmt
cargo clippy --all-targets           # READ the output, see below
cargo test --lib                     # zero failures
cd ..
bun run format:frontend
bun run lint
bunx tsc --noEmit
bun run check:translations           # all languages complete
```

`cargo check` in step 3 settles `Cargo.lock`, but nothing settles the frontend
dependencies. If upstream bumped a package and you skip `bun install`, the
failures land in `tsc` and `lint` and read as merge damage.

**Clippy is the one gate whose exit status means nothing.** Without `-D warnings`
it exits 0 no matter how many warnings it prints, and adding `-D warnings` is not
the fix: it would also fail on upstream's own pre-existing ones, so the gate would
be red on every sync and get ignored. Read the output and look for anything
pointing into `src-tauri/src/cloud/`. Judge this one on what it printed, not on
whether it succeeded.

Judge the rest on zero failures, not on matching a remembered count. Upstream adds
and removes tests, so a fixed number goes stale exactly like a line number does.

## Step 5: invariants, the step that actually earns its keep

```bash
node .claude/skills/sync-upstream/fork-guard.mjs verify
```

Checks for behaviour that **compiles while being wrong**. Every gate above can
pass with any of these broken. Exit 1 means the fork is degraded even though the
build is green; exit 2 means the environment is wrong, not the code. Read the
FAIL line: each says what breaks for the user, not just what is missing.

One of them checks `FORK.md` rather than the code: if the merge left the fork
editing an upstream file the map does not list, it fails and names the file. A
map that quietly drops an entry is how the *next* sync underestimates its own
blast radius.

It runs under `node` or `bun`, on any OS, with no dependencies, and it shells out
to nothing except `git`. That is deliberate. The first version was a shell script
using ripgrep, which is absent from a non-interactive shell on Windows, so every
check passed by finding nothing. A checker that can only report success is worse
than no checker.

If a check fails because the fork legitimately changed shape, a command was added
or code was refactored, **update the script in the same commit**. A check nobody
trusts gets ignored, and then it protects nothing.

If you touch either the checks or the code they point at, prove they still work:

```
node .claude/skills/sync-upstream/guard-test.mjs
```

It breaks the target of every invariant in turn and requires each check to go red,
then restores the tree. A check that stays green while the thing it guards is gone
reads exactly like a check that works, and no amount of rereading the script tells
them apart. Expect all mutations caught. Anything else means an invariant is
watching nothing, which is worse than not having it, because it reports safety.

## Step 6: keep the map honest

Step 5 fails if `FORK.md` no longer lists an upstream file the fork edits. Fix it
**in the merge commit**, not later.

Things the script cannot check, so check them yourself whenever the merge changed
the fork's own behaviour:

- a new fork command means adding its name to `EXPECTED_COMMANDS` in
  `fork-guard.mjs`, and to `guard-test.mjs` if it becomes an anchor
- a new hook in an upstream file means both `FORK.md`'s table and the `HOOKS` set
  in `fork-guard.mjs` need it, or the survey will tag that file "additive" and
  invite exactly the careless resolution this procedure exists to prevent
- a new upstream file where "keep both" is the wrong answer belongs in `OURS` in
  `fork-guard.mjs` as well as in step 3's table
- the per-file counts in step 3's table are a merge checklist, not trivia. If the
  fork gains a settings field, correct the count in the same commit, or a future
  merge will drop one and the table will agree that nothing is missing

## Step 7: what the checks still cannot see

Only reachable by running the app. Do these before calling a sync done:

1. **Post-processing with an empty rotation** still cleans up, via upstream's
   single key.
2. **Cloud speech on, dictate** and watch GPU memory. Flat means the deferred
   model load survived.
3. **One key with two different models**: both rows survive a save.
4. **Apple Intelligence**, on a Mac only.

## Step 8: report

Tell the user, in this order:

- how many upstream commits came in, and anything notable among them
- which files conflicted and how each was resolved
- gate results, and the invariant count
- **anything you resolved by judgement rather than mechanically**, called out
  separately; that is where a wrong call hides
- both backup refs, and that they can be deleted once they are satisfied

## Aborting

**If a resolution was wrong, purge it before anything else.** `rerere` recorded it
the moment you staged the file and will replay it on every retry, so an abort
alone reproduces the same mistake:

```
node -e "require('node:fs').rmSync('.git/rr-cache',{recursive:true,force:true})"
```

Neither documented alternative does this job, both verified against git 2.52:
`git rerere clear` leaves the recorded resolution in place and the next merge
replays it regardless, and `git rerere forget <path>` fails with "no remembered
resolution" even while the path is unmerged, because it matches on the original
conflict markers and rerere has already overwritten them with the resolution.
Deleting the cache costs nothing: it is per-clone and rebuilds itself.

Then unwind, mid-merge:

```
git merge --abort
```

Or after committing the merge:

```
git reset --hard backup/pre-sync
```

**Either way, put `main` back too.** Step 2 fast-forwarded it and neither command
above touches it:

```
git branch -f main backup/pre-sync-main
```

Skip this and the next survey reports zero incoming commits, which step 1 reads as
a misconfigured remote. Run it from the fork branch, not from `main`.

Never force-push to recover. The branch has no upstream tracking and nothing
downstream depends on it, so a local reset is always sufficient.

Once the sync is good, tell the user they can drop the safety net:

```
git branch -D backup/pre-sync backup/pre-sync-main
```

## Related

**Read `FORK.md` in the repo root first if you do not already know this fork.**
It is the map: what the fork adds, how it fits together, every upstream file it
touches and what it holds there, and the decisions taken deliberately. Two
sections matter most during a sync. "Decisions taken deliberately" and
"Deliberately not built" exist so a merge does not quietly undo a choice that
looks like a mistake out of context.
