#!/usr/bin/env node
// Tests fork-guard.mjs. Breaks the target of every invariant the way a bad merge
// would, and asserts the matching check goes red. A check that stays green while
// the thing it guards is gone is guarding nothing, and reads exactly like a check
// that works. Reviewing the guard by eye cannot tell those apart. This can.
//
// Anchors are substrings, never line numbers, so an upstream sync moving code
// around does not turn this into the next stale file.
//
// Mutations are applied to the working tree and restored immediately. It refuses
// to start unless the tree is clean, and fails loudly if it cannot put it back.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const GUARD = ".claude/skills/sync-upstream/fork-guard.mjs";

const git = (...args) =>
  execFileSync("git", args, { encoding: "utf8" }).trim();
const root = git("rev-parse", "--show-toplevel");

// Each entry: the check's exact name, and text whose removal must break it.
const MUTATIONS = [
  ["upstream post-processing still has a caller",
    "src-tauri/src/fork/hooks.rs", "crate::actions::post_process_transcription"],
  ["post-processing gates on the rotation, not just the toggle",
    "src-tauri/src/fork/cloud/post_process.rs", "cloud_bindings.post_process.entries.is_empty()"],
  ["local model load is deferred when cloud serves speech",
    "src-tauri/src/fork/hooks.rs", "return false;"],
  ["the fallback path loads the model before transcribing",
    "src-tauri/src/fork/hooks.rs", "tm.initiate_model_load();"],
  ["cloud transcripts get the local text cleanup",
    "src-tauri/src/fork/hooks.rs", "apply_text_post_processing"],
  ["streaming is suppressed while cloud speech is on",
    "src-tauri/src/actions.rs", "let model_supports_streaming = local_engine"],
  ["the transcription path reaches the fork seam",
    "src-tauri/src/actions.rs", "crate::fork::hooks::transcribe"],
  ["the post-processing path reaches the fork seam",
    "src-tauri/src/actions.rs", "crate::fork::hooks::post_process"],
  ["Apple Intelligence is reachable through the pool",
    "src-tauri/src/fork/cloud/post_process.rs", "apple_intelligence_completion"],
  ["cloud STT errors go through the URL sanitizer",
    "src-tauri/src/fork/cloud/stt.rs", "report_reqwest_error"],
  ["transport failures are classified non-striking",
    "src-tauri/src/fork/cloud/error.rs", "FailureClass::Unreachable"],
  ["a shared quota benches every bucket on that credential",
    "src-tauri/src/fork/cloud/pool.rs", "shared_cooldowns()"],
  ["the save path de-duplicates on credential AND model",
    "src-tauri/src/fork/cloud/binding.rs", "seen.insert((entry.credential_id"],
  ["deleting a template clears entries that named it",
    "src-tauri/src/shortcut/mod.rs", "entry.prompt_id = None"],
  ["the updater does not point at upstream",
    "src-tauri/tauri.conf.json", "ehsan18t/Handy-Plus"],
  ["the fork i18n namespace is registered",
    "src/i18n/index.ts", "resources[langCode].fork = module.default;"],
  ["the fork module is declared",
    "src-tauri/src/lib.rs", "pub mod fork;"],
  ["the degraded event is registered",
    "src-tauri/src/lib.rs", "CloudDegradedEvent"],
  ["the history regenerate command stays registered",
    "src-tauri/src/lib.rs", "commands::history::regenerate_history_entry_post_process"],
  ["all 8 fork commands are registered",
    "src-tauri/src/lib.rs", "fork::hooks::add_cloud_credential"],
  ["FORK.md lists every upstream file the fork edits",
    "FORK.md", "src/components/ui/Badge.tsx"],
];

// Checks that fire on text being PRESENT rather than absent, so they are broken
// by putting the bad thing back rather than by taking the good thing away.
const REINTRODUCE = [
  ["cloud STT does not format raw reqwest errors",
    "src-tauri/src/fork/cloud/stt.rs",
    '\nfn leak() { ApiError::transport(format!("Speech request failed: {e}")) }\n'],
  ["upstream files reach the fork only through the seam",
    "src-tauri/src/settings.rs",
    "\nfn reaches_past() -> Option<crate::fork::cloud::Capability> { None }\n"],
  ["the updater lists no endpoint but this fork's",
    "src-tauri/tauri.conf.json",
    '\n"https://github.com/cjpais/Handy/releases/latest/download/latest.json"\n'],
];

function guard() {
  try {
    return execFileSync("node", [GUARD, "verify"], { cwd: root, encoding: "utf8" });
  } catch (error) {
    return (error.stdout ?? "") + (error.stderr ?? "");
  }
}

function statusOf(output, check) {
  for (const line of output.split("\n")) {
    const match = line.match(/^\s*(PASS|FAIL)\s+(.*?)\s*$/);
    if (match && match[2] === check) return match[1];
  }
  return "ABSENT";
}

// Only the files this rewrites have to be clean. Restoring is done from memory,
// so a crash between write and restore would lose uncommitted work in exactly
// those files and nowhere else. Anything else being dirty is not this test's
// business, including the guard itself while it is being edited.
const targets = [...new Set([...MUTATIONS, ...REINTRODUCE].map((m) => m[1]))];
const dirtyTargets = git("status", "--porcelain", "--", ...targets);
if (dirtyTargets) {
  console.error(`these are rewritten by this test and have uncommitted changes:\n${dirtyTargets}`);
  process.exit(2);
}

const names = [...MUTATIONS.map((m) => m[0]), ...REINTRODUCE.map((m) => m[0])];
const baseline = guard();
const unready = names.filter((name) => statusOf(baseline, name) !== "PASS");
if (unready.length) {
  console.error("baseline is not green, so no mutation result would mean anything:");
  for (const name of unready) console.error(`  ${statusOf(baseline, name)}  ${name}`);
  process.exit(2);
}
console.log(`baseline: ${names.length} checks green\n`);

let blind = 0;
const attempt = (name, file, mutate) => {
  const path = join(root, file);
  const original = readFileSync(path, "utf8");
  try {
    writeFileSync(path, mutate(original));
    const status = statusOf(guard(), name);
    if (status === "FAIL") {
      console.log(`  caught   ${name}`);
      return;
    }
    blind++;
    console.log(`  BLIND    ${name}  [stayed ${status}]`);
  } catch (error) {
    blind++;
    console.log(`  ERROR    ${name}: ${error.message}`);
  } finally {
    writeFileSync(path, original);
  }
};

const hit = (source, anchor, file) => {
  const lines = source.split("\n");
  if (!lines.some((line) => line.includes(anchor)))
    throw new Error(`nothing in ${file} contains "${anchor}"; the anchor has drifted`);
  return lines;
};

console.log("deleted:");
for (const [name, file, anchor] of MUTATIONS) {
  attempt(name, file, (source) =>
    hit(source, anchor, file)
      .filter((line) => !line.includes(anchor))
      .join("\n"),
  );
}

// The mutation this test did not have when it first reported 18/18 green. A
// conflict resolved by commenting one side out leaves every identifier exactly
// where a text search expects it, so a checker that does not strip comments
// reports the fork intact while the code is gone.
console.log("\ncommented out:");
const commentOut = (file, line) =>
  file.endsWith(".md") ? `<!-- ${line} -->` : `// ${line}`;
for (const [name, file, anchor] of MUTATIONS) {
  attempt(name, file, (source) =>
    hit(source, anchor, file)
      .map((line) => (line.includes(anchor) ? commentOut(file, line) : line))
      .join("\n"),
  );
}

console.log("\nre-introduced:");
for (const [name, file, addition] of REINTRODUCE) {
  attempt(name, file, (source) => source + addition);
}

const total = MUTATIONS.length * 2 + REINTRODUCE.length;
console.log(`\n${total - blind}/${total} mutations caught`);

const dirty = git("status", "--porcelain", "--", ...targets);
if (dirty) {
  console.error(`\ntree NOT restored, fix before committing:\n${dirty}`);
  process.exit(2);
}
process.exit(blind ? 1 : 0);
