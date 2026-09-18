#!/usr/bin/env node
// Fork guard for Handy-Plus. Runs under node or bun, on any OS, with no deps.
//
//   fork-guard.mjs survey    what an upstream sync would collide with
//   fork-guard.mjs verify    invariants that compile while being wrong
//   fork-guard.mjs           both
//
//   exit 0  fine
//   exit 1  an invariant broke: the fork is degraded even if everything compiles
//   exit 2  could not run
//
// Deliberately not a shell script. The first version used ripgrep, which is
// absent from a non-interactive shell on Windows, so every check passed by
// finding nothing. Shelling out is how this file lies to you.

import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const color = process.stdout.isTTY && !process.env.NO_COLOR;
const red = (s) => (color ? `\x1b[31m${s}\x1b[0m` : s);
const green = (s) => (color ? `\x1b[32m${s}\x1b[0m` : s);
const dim = (s) => (color ? `\x1b[2m${s}\x1b[0m` : s);
const bold = (s) => (color ? `\x1b[1m${s}\x1b[0m` : s);

function git(...args) {
  try {
    return execFileSync("git", args, { encoding: "utf8" }).trim();
  } catch {
    return null;
  }
}

// For the diffs the checks reason about, "git failed" and "git found nothing"
// must not look alike: both arrive as an empty list, and an empty list reads as
// a clean fork. Anything derived from a diff goes through here.
function gitLines(...args) {
  const out = git(...args);
  if (out === null) {
    console.error(`git ${args.join(" ")} failed; cannot judge the fork`);
    process.exit(2);
  }
  return out.split("\n").filter(Boolean);
}

const root = git("rev-parse", "--show-toplevel");
if (!root) {
  console.error("not inside a git repository");
  process.exit(2);
}

const read = (file) => {
  const path = join(root, file);
  return existsSync(path) ? readFileSync(path, "utf8") : null;
};

// ---------------------------------------------------------------- invariants

// Whitespace is normalised before matching so a pattern can span lines without
// caring how rustfmt or prettier chose to wrap it that week.
const flat = (text) => text.replace(/\s+/g, " ");

// Commenting a side out is how a hand-resolved conflict most often disables code,
// and it leaves every identifier exactly where a text search expects to find it.
// Only line-leading comments are stripped: a trailing `//` cannot be told apart
// from the one inside an https:// literal, and tauri.conf.json is all URLs.
const uncomment = (text) =>
  text
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .split("\n")
    .filter((line) => !/^\s*\/\//.test(line))
    .join("\n");

const INVARIANTS = [
  {
    name: "upstream post-processing still has a caller",
    file: "src-tauri/src/fork/hooks.rs",
    want: /NotEngaged => \{ crate::actions::post_process_transcription/,
    why: "An empty rotation must fall through to upstream's single key. Without this, upgrading silently loses post-processing and pastes raw transcripts.",
  },
  {
    name: "post-processing gates on the rotation, not just the toggle",
    file: "src-tauri/src/fork/cloud/post_process.rs",
    want: /cloud_bindings\.post_process\.entries\.is_empty\(\)/,
    why: "Gating on post_process_enabled alone makes every existing install degrade.",
  },
  {
    name: "local model load is deferred when cloud serves speech",
    file: "src-tauri/src/fork/hooks.rs",
    // The early return has to come BEFORE the load, which is the whole
    // behaviour: matching both in any order would pass on code that loads the
    // model and then decides not to.
    want: /is_enabled\(settings\) \{ return false; \}.*initiate_model_load/,
    why: "Otherwise every dictation loads a multi-gigabyte model into VRAM that nothing uses.",
  },
  {
    name: "the fallback path loads the model before transcribing",
    file: "src-tauri/src/fork/hooks.rs",
    want: /tm\.initiate_model_load\(\); tm\.transcribe\(samples\)/,
    why: "transcribe() errors rather than loading, so fallback would hard-fail.",
  },
  {
    name: "cloud transcripts get the local text cleanup",
    file: "src-tauri/src/fork/hooks.rs",
    want: /apply_text_post_processing/,
    why: "Custom words and filler stripping would apply to local output only.",
  },
  {
    name: "streaming is suppressed while cloud speech is on",
    file: "src-tauri/src/actions.rs",
    want: /let model_supports_streaming = local_engine/,
    why: "A streaming engine finalizes first and its text wins before cloud ever runs.",
  },
  // Reachability. Every check above this pair reads fork/hooks.rs, and a hook
  // nothing calls is still a perfectly correct hook: those checks would all stay
  // green while the fork did nothing at all. These two are the only thing tying
  // the seam back to the upstream file that is supposed to use it.
  {
    name: "the transcription path reaches the fork seam",
    file: "src-tauri/src/actions.rs",
    want: /crate::fork::hooks::transcribe\(&ah, samples, cancel_generation\)/,
    why: "Cloud speech becomes unreachable: the toggle stays on, the rotation stays configured, and every dictation silently goes to the local model.",
  },
  {
    name: "the post-processing path reaches the fork seam",
    file: "src-tauri/src/actions.rs",
    want: /crate::fork::hooks::post_process\(app, &settings, &final_text\)/,
    why: "The rotation is bypassed and every cleanup goes to upstream's single key, so a configured pool is silently ignored.",
  },
  {
    name: "Apple Intelligence is reachable through the pool",
    file: "src-tauri/src/fork/cloud/post_process.rs",
    want: /apple_intelligence_completion/,
    why: "It is native Swift rather than HTTP, so a provider refactor drops it first.",
  },
  {
    name: "cloud STT errors go through the URL sanitizer",
    file: "src-tauri/src/fork/cloud/stt.rs",
    want: /report_reqwest_error/,
    why: "reqwest's Display appends the raw URL, which can carry a key in userinfo.",
  },
  {
    name: "cloud STT does not format raw reqwest errors",
    file: "src-tauri/src/fork/cloud/stt.rs",
    reject: /ApiError::transport\(format!\("Speech request failed/,
    why: "That leaks the endpoint URL into handy.log and into a user-facing toast.",
  },
  {
    name: "transport failures are classified non-striking",
    file: "src-tauri/src/fork/cloud/error.rs",
    want: /FailureClass::Unreachable/,
    why: "Striking them benched every key for six hours after three offline dictations.",
  },
  {
    name: "a shared quota benches every bucket on that credential",
    file: "src-tauri/src/fork/cloud/pool.rs",
    // The call alone is not enough: the deadlines have to be merged into the
    // per-candidate state or the query runs and changes nothing.
    want: /shared_cooldowns().*cooldown_until_ms = Some/,
    why: "A provider that meters speech and cleanup from one allowance would keep being sent requests it has already refused, burning a strike per dictation on a key that is simply out.",
  },
  {
    name: "the save path de-duplicates on credential AND model",
    file: "src-tauri/src/fork/cloud/binding.rs",
    want: /seen\.insert\(\(entry\.credential_id\.clone\(\), entry\.model/,
    why: "De-duplicating on the credential alone silently drops the second model, which is the whole point of per-entry models.",
  },
  {
    name: "deleting a template clears entries that named it",
    file: "src-tauri/src/shortcut/mod.rs",
    want: /entry\.prompt_id = None/,
    why: "A dangling prompt_id fails every request and benches a healthy key.",
  },
  {
    name: "the updater does not point at upstream",
    file: "src-tauri/tauri.conf.json",
    want: /ehsan18t\/Handy-Plus/,
    why: "Taking upstream's tauri.conf.json replaces this build with vanilla Handy.",
  },
  {
    name: "the updater lists no endpoint but this fork's",
    file: "src-tauri/tauri.conf.json",
    reject: /cjpais\/Handy/,
    why: "Tauri tries endpoints in order and the two repos share a pubkey, so upstream's URL sitting alongside ours installs vanilla Handy over this build and signature-verifies while doing it.",
  },
  {
    name: "the fork i18n namespace is registered",
    file: "src/i18n/index.ts",
    want: /resources\[langCode\]\.fork = /,
    why: "Fork strings render as raw keys without it.",
  },
  {
    name: "the fork module is declared",
    file: "src-tauri/src/lib.rs",
    want: /pub mod fork;/,
    why: "Nothing in src-tauri/src/fork/cloud/ is compiled without it.",
  },
  {
    // Not in EXPECTED_COMMANDS: that list matches `fork::hooks::` names, and this
    // command deliberately lives in upstream's own history module (see the
    // exception in docs/FORK_RECIPE.md). Nothing else would notice a merge in
    // lib.rs dropping the line.
    name: "the history regenerate command stays registered",
    file: "src-tauri/src/lib.rs",
    want: /commands::history::regenerate_history_entry_post_process/,
    why: "The Post Process tab's regenerate button invokes it. Unregistered, the button fails at runtime only.",
  },
  {
    name: "the degraded event is registered",
    file: "src-tauri/src/lib.rs",
    want: /CloudDegradedEvent/,
    why: "Every fallback becomes invisible to the user without it.",
  },
];

// lib.rs holds a command list upstream also edits, so a merge can drop one of
// ours while still compiling. The UI then fails at runtime only.
//
// Names rather than a count. A count is satisfied by a duplicate standing in for
// a missing entry, and by a registration that was commented out rather than
// deleted, which is the shape a hand-resolved conflict most often takes.
const EXPECTED_COMMANDS = [
  "add_cloud_credential",
  "update_cloud_credential",
  "delete_cloud_credential",
  "test_cloud_credential",
  "set_cloud_binding",
  "get_cloud_credential_status",
  "clear_cloud_cooldown",
  "get_cloud_providers",
];

const MAP_CHECK = "FORK.md lists every upstream file the fork edits";

function verify() {
  console.log(`\n${bold("Fork invariants")}\n`);
  let pass = 0;
  const failures = [];

  const record = (okay, name, why) => {
    if (okay) {
      console.log(`  ${green("PASS")}  ${name}`);
      pass++;
    } else {
      console.log(`  ${red("FAIL")}  ${name}`);
      console.log(`        ${dim(why)}`);
      failures.push(name);
    }
  };

  for (const check of INVARIANTS) {
    const source = read(check.file);
    if (source === null) {
      record(false, check.name, `missing file: ${check.file}`);
      continue;
    }
    const text = flat(uncomment(source));
    const found = (check.want ?? check.reject).test(text);
    record(check.want ? found : !found, check.name, check.why);
  }

  const lib = uncomment(read("src-tauri/src/lib.rs") ?? "");
  const registered = new Set(
    [...lib.matchAll(/fork::hooks::(\w+)/g)].map((match) => match[1]),
  );
  const missing = EXPECTED_COMMANDS.filter((name) => !registered.has(name));
  record(
    missing.length === 0,
    `all ${EXPECTED_COMMANDS.length} fork commands are registered`,
    `Not registered: ${missing.join(", ")}. A merge in lib.rs dropped it, or a command was added without listing it in EXPECTED_COMMANDS in this script. The UI calls it and fails at runtime.`,
  );

  // The seam. Every upstream file reaches the fork through `fork::hooks` and
  // nothing else, so a merge conflict in an upstream file is always about a
  // single call rather than about fork internals the resolver has to understand.
  // The rule is only worth having if it is checked: the first time someone
  // reaches past it for "just one type", the next person copies that, and the
  // conflict surface starts growing again where nobody is looking.
  const reachesPastSeam = gitLines(
    "diff",
    "--diff-filter=M",
    "--name-only",
    "main...HEAD",
  )
    .filter((file) => file.endsWith(".rs"))
    .filter((file) => {
      const source = read(file);
      if (source === null) return false;
      return [...uncomment(source).matchAll(/fork::(\w+)/g)].some(
        (match) => match[1] !== "hooks",
      );
    });
  record(
    reachesPastSeam.length === 0,
    "upstream files reach the fork only through the seam",
    `Reaching past fork::hooks: ${reachesPastSeam.join(", ")}. Re-export what is needed from fork::hooks instead. Every extra path an upstream file names is another thing a merge conflict makes the resolver reason about.`,
  );

  // FORK.md is what a newcomer and every future sync reads to judge risk. If it
  // stops listing a file the fork actually edits, the next sync underestimates
  // its own blast radius, which is worse than having no map at all.
  // Markdown's comment is the HTML one, and a row hidden inside it still reads
  // as present to a plain substring search.
  const mapSource = read("FORK.md");
  const map = mapSource === null ? null : mapSource.replace(/<!--[\s\S]*?-->/g, "");
  const modified = gitLines(
    "diff",
    "--diff-filter=M",
    "--name-only",
    "main...HEAD",
  );

  if (map === null) {
    record(false, MAP_CHECK, "FORK.md is missing.");
  } else if (!modified.length) {
    // Not a pass. The fork edits upstream files by definition, so an empty set
    // means this ran somewhere it cannot see them: on main, or on a detached
    // checkout mid-merge. Reporting green there is the lie this file exists to
    // avoid.
    record(false, MAP_CHECK, "No upstream edits found at all. Run this on the fork branch.");
  } else {
    const absent = modified.filter((file) => !map.includes(file));
    record(
      absent.length === 0,
      MAP_CHECK,
      `Not in the map: ${absent.join(", ")}. Add them to the upstream-files table, or the next sync will not know they carry fork changes.`,
    );
  }

  console.log(`\npassed ${pass}, failed ${failures.length}`);
  if (failures.length) {
    console.log(
      `\n${red("This build can still compile and pass every gate. It is degraded.")}`,
    );
  }
  return failures.length === 0;
}

// -------------------------------------------------------------------- survey

const GENERATED = new Set(["src/bindings.ts", "src-tauri/Cargo.lock"]);
const HOOKS = new Set([
  "src-tauri/src/actions.rs",
  "src-tauri/src/settings.rs",
  "src-tauri/src/llm_client.rs",
  "src-tauri/src/lib.rs",
]);
// Files where "keep both sides", the default everywhere else, is the wrong
// answer and produces a working build that is subtly not this fork.
const OURS = new Set(["src-tauri/tauri.conf.json", ".gitignore"]);

function survey() {
  console.log(`\n${bold("Upstream sync survey")}\n`);

  // Counted, never remembered. Every hard-coded total in this project has gone
  // stale, so the footprint is derived on the spot.
  const added = gitLines(
    "diff",
    "--diff-filter=A",
    "--name-only",
    "main...HEAD",
  ).length;
  const modified = gitLines(
    "diff",
    "--diff-filter=M",
    "--name-only",
    "main...HEAD",
  ).length;
  console.log(
    `  fork footprint: ${added} new files (never conflict), ${modified} upstream files (all the risk)`,
  );

  const remote = git("remote", "get-url", "upstream");
  if (!remote) {
    console.log(`  ${red("no 'upstream' remote")}`);
    return "config";
  }
  if (!/cjpais\/Handy(\.git)?$/i.test(remote)) {
    console.log(`  ${red("upstream points at")} ${remote}`);
    console.log(
      `        ${dim("Expected cjpais/Handy. A sync will report nothing rather than fail.")}`,
    );
    return "config";
  }
  console.log(`  upstream: ${remote}`);

  const incoming = git("rev-list", "--count", "main..upstream/main");
  if (incoming === null) {
    console.log(`  ${dim("no upstream/main; run: git fetch upstream")}`);
    return "config";
  }
  console.log(`  incoming commits: ${incoming}`);
  if (incoming === "0") {
    console.log(`\n  ${green("already up to date")}`);
    return "ok";
  }

  const log = git("log", "main..upstream/main", "--oneline");
  if (log) console.log(`\n${log.replace(/^/gm, "    ")}`);

  // Three dots, so this is what upstream changed since the fork point rather
  // than every difference between the two tips. They agree only while main is
  // an ancestor of upstream/main, and the moment it is not, two dots would
  // report the fork's own files as upstream churn.
  const theirs = new Set(
    gitLines("diff", "--name-only", "main...upstream/main"),
  );
  const ours = gitLines(
    "diff",
    "--diff-filter=M",
    "--name-only",
    "main...HEAD",
  );
  const collisions = ours.filter((file) => theirs.has(file));

  console.log(`\n  ${bold("Files both sides touch")}\n`);
  if (!collisions.length) {
    console.log(`    ${green("none: a clean sync")}`);
  } else {
    for (const file of collisions.sort()) {
      const tag = GENERATED.has(file)
        ? red("regenerate, never merge")
        : HOOKS.has(file)
          ? red("carries fork hooks, read the whole hunk")
          : OURS.has(file)
            ? red("ours wins, do not union")
            : dim("additive, keep both sides");
      console.log(`    ${file.padEnd(42)} ${tag}`);
    }
    const risky = collisions.filter((f) => HOOKS.has(f) || OURS.has(f)).length;
    console.log(
      `\n  ${collisions.length} colliding, ${collisions.filter((f) => GENERATED.has(f)).length} generated, ${risky} needing judgement`,
    );
  }
  return "ok";
}

// ---------------------------------------------------------------------- main

const mode = process.argv[2] ?? "all";
if (!["survey", "verify", "all"].includes(mode)) {
  console.error(`unknown mode: ${mode}\nuse: survey | verify | all`);
  process.exit(2);
}

let degraded = false;
let misconfigured = false;

if (mode === "survey" || mode === "all") {
  if (survey() === "config") misconfigured = true;
}
if (mode === "verify" || mode === "all") {
  if (!verify()) degraded = true;
}

// A broken invariant and a broken setup need different reactions, so they get
// different exit codes: fix the fork, versus fix the environment.
if (misconfigured) process.exit(2);
process.exit(degraded ? 1 : 0);
