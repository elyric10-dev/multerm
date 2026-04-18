---
name: Don't run cargo build
description: User doesn't want Claude to run cargo build; they build themselves
type: feedback
---

Don't run `cargo build` (or any build command). Write the code, then stop. The user builds and tests it themselves.

**Why:** User has interrupted every `cargo build` invocation. They prefer to own the build step.

**How to apply:** After making code changes, stop and explain what was done without running a build.
