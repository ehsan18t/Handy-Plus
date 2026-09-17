//! Everything this fork adds, in one namespace upstream does not have.
//!
//! Two rules keep the fork cheap to rebase, and `fork-guard.mjs verify`
//! checks both:
//!
//! 1. Fork code lives here. A file under `fork/` can never conflict with
//!    upstream, because upstream has no file at that path.
//! 2. Upstream files import `fork::hooks` and nothing else from here, and
//!    call it on a single line. A one-line call is a mechanical conflict to
//!    resolve; an inlined block is a judgement call, and judgement calls are
//!    where a merge quietly reverts a decision.
//!
//! Adding a feature: see `docs/FORK_RECIPE.md`.

pub mod cloud;
pub mod hooks;
