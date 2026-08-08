# Custom Maki Fork Workflow

This document describes the branch structure and workflow for maintaining a custom [maki](https://github.com/tontinton/maki) fork with independent feature branches that can be selectively combined.

## Overview

This fork uses a **modular feature branch strategy** where:

- `main` tracks upstream maki
- Each feature lives in its own independent branch based on `main`
- `alberto/my-fork` is a merge commit that combines all desired features
- `alberto/fork-customizations` holds fork-specific additions (this doc)
- Jujutsu (jj) is used for version control alongside Git

This approach allows:

- Easy updates when upstream moves
- Selective feature inclusion (enable/disable features by changing the merge)
- Clean separation of concerns
- Simple conflict resolution per-feature

We do not intend to upstream these changes. When upstream happens to fix the same thing, the matching branch gets retired instead of rebased forever.

## Remotes

| Remote | URL | Purpose |
|--------|-----|---------|
| `origin` | `git@github.com:dashed/maki.git` | Our fork, where all branches are pushed |
| `upstream` | `git@github.com:tontinton/maki.git` | Read from this to follow upstream |

`main` tracks `main@origin`. `main@upstream` stays untracked on purpose, so upstream never moves our bookmark behind our back. We move it ourselves during an update.

## Branch Structure

```
main (upstream)
│
├── alberto/fork-customizations
│   └── Git hash in `maki --version`, this doc
│
├── alberto/openrouter-auth
│   └── Register OpenRouter as a built-in provider
│
├── alberto/effort-levels
│   └── Per-model reasoning effort, provider routing, speed stats
│
├── alberto/modal-hints
│   └── Say how to close pickers and modals
│
└── alberto/my-fork (integration merge)
    └── Combines all features + customizations
```

### Branch Descriptions

| Branch | Purpose | Commits |
|--------|---------|:-------:|
| `main` | Tracks upstream maki | — |
| `alberto/fork-customizations` | Git hash in `--version`, this doc | 3 |
| `alberto/openrouter-auth` | OpenRouter built-in registration | 1 |
| `alberto/effort-levels` | Effort levels, `/provider` routing, per-provider speed | 6 |
| `alberto/modal-hints` | Close hints on pickers and modals | 1 |
| `alberto/my-fork` | Combined features | merge |

### Retired Branches

| Branch | Reason | Date |
|--------|--------|------|
| — | — | — |

## What Each Feature Branch Does

### alberto/openrouter-auth

OpenRouter was the only provider missing an `inventory::submit!(BuiltInProvider {...})` entry. Two things followed from that. `maki auth login openrouter` bailed with `unknown provider 'openrouter'`, and the slug never appeared in `maki auth status`. The models.dev catalog could not cover for it either, because `catalog.rs` only accepts the npm packages `@ai-sdk/openai-compatible` and `@ai-sdk/anthropic`, while OpenRouter ships `@openrouter/ai-sdk-provider`.

Registering it alone would have been a trap. `OpenRouter::new` read its key with `KeyPool::from_env`, so a login would have written `~/.local/state/maki/auth/openrouter.json`, printed a cheerful checkmark, and then every request would still have failed with `OPENROUTER_API_KEY not set`. The branch also switches to `KeyPool::resolve`, which is what the other fourteen providers use: environment variable first, then that saved credentials file, then `providers.toml`.

Because `resolve` tries the environment first, anyone already exporting `OPENROUTER_API_KEY` sees no change at all.

## Jujutsu (jj) Setup

This repo is colocated, meaning both `jj` and `git` commands work.

### Why jj?

- **Automatic rebasing**: when you update a parent, descendants auto-rebase
- **First-class conflicts**: conflicts are stored in commits, resolve when convenient
- **Operation log**: every operation can be undone with `jj undo`
- **Change IDs**: stable identifiers that survive rebases (unlike git commit hashes)
- **Multi-parent commits**: native support for merge commits with many parents

### Setting it up from scratch

```bash
git remote add upstream git@github.com:tontinton/maki.git
git fetch upstream
jj git init --colocate
jj bookmark track main@origin
```

## Updating from Upstream

### Step 1: Fetch upstream changes

```bash
jj git fetch --all-remotes
```

### Step 2: Note the old main, then move main

```bash
old_main=$(jj log -r main --no-graph -T 'commit_id.short()')
jj bookmark set main -r main@upstream
```

### Step 3: Rebase feature branches onto the new main

Rebase in order of conflict risk, lowest first:

```bash
jj rebase -s "roots(${old_main}..alberto/fork-customizations)" -d main
jj rebase -s "roots(${old_main}..alberto/modal-hints)" -d main
jj rebase -s "roots(${old_main}..alberto/openrouter-auth)" -d main
jj rebase -s "roots(${old_main}..alberto/effort-levels)" -d main
```

### Step 4: Resolve any conflicts

```bash
jj log -r 'conflicts()'

# For each conflicted commit:
jj new <conflicted-commit-id>
# Edit files to resolve
jj squash -u

# For lockfiles, take upstream and let cargo regenerate:
jj restore --from main Cargo.lock
```

Resolving the earliest conflicted commit often cascades and fixes its descendants automatically.

### Step 5: Rebuild the integration branch

```bash
jj new alberto/openrouter-auth alberto/fork-customizations \
       alberto/effort-levels alberto/modal-hints \
  -m "integration: combine fork branches"
jj bookmark set alberto/my-fork --allow-backwards -r @
```

### Step 6: Verify and push

```bash
cargo +1.97.1 clippy --all --tests --target-dir target -- -D warnings
cargo +1.97.1 test --workspace --target-dir target
cargo +1.97.1 run -p maki-docgen --target-dir target -- --check

jj git push --bookmark main
jj git push --bookmark alberto/fork-customizations
jj git push --bookmark alberto/openrouter-auth
jj git push --bookmark alberto/my-fork

jj git export
git checkout alberto/my-fork
```

## Adding a New Feature

> **Start with `jj new`, always.** Rebuilding the integration merge leaves the
> working copy sitting on that merge, so editing files right then hands the new
> commit every one of the merge's parents. The branch you bookmark it onto
> quietly swallows the other branches, and independent branches are the entire
> point of this layout. It has already happened twice here.
>
> The tell is a feature branch whose `main..branch` log lists commits belonging
> to other branches. Check with:
>
> ```bash
> jj log -r 'main..alberto/some-feature' --no-graph \
>   -T 'commit_id.short() ++ "  " ++ description.first_line() ++ "\n"'
> ```
>
> To repair, find the commit with more than one parent and give it a single one:
>
> ```bash
> jj log -r <suspect> --no-graph \
>   -T 'commit_id.short() ++ " <- " ++ parents.map(|p| p.commit_id().short()).join(", ") ++ "\n"'
> jj rebase -s <the-merge-y-commit> -d <the-real-branch-tip>
> ```
>
> `-s` brings its descendants along, so one rebase straightens the whole chain.
> Then reset the bookmark, rebuild the merge, and force push.

```bash
jj new main -m "feat: description of feature"
jj bookmark create alberto/new-feature
# make changes, they are tracked automatically
```

Then fold it into the integration branch:

```bash
jj new alberto/openrouter-auth alberto/fork-customizations \
       alberto/effort-levels alberto/modal-hints alberto/new-feature \
  -m "integration: combine fork branches"
jj bookmark set alberto/my-fork --allow-backwards -r @
```

### Sibling or stacked?

Siblings off `main` only stay quiet when they touch different files. Two
features that each register a slash command both append to `BUILTIN_COMMANDS`
and to the const block in `maki-ui/src/app/mod.rs`, and that pair conflicts on
every rebase and every time the merge is rebuilt. Provider routing started as a
sibling for exactly that reason and produced five conflicts before landing on
`alberto/effort-levels` instead, where it belonged anyway.

Stack when two features share a surface or one uses the other's code. Keep them
siblings when they can genuinely be dropped one at a time, which is what the
Retired Branches table is for. Reusing a const from another branch is the
clearest sign you have picked wrong.

## The Integration Branch (my-fork)

`alberto/my-fork` is a **merge commit with multiple parents**. It combines all feature branches into a single working build. When any parent branch is updated, recreate the merge with the command above.

Keeping `fork-customizations` as its own parent rather than baking it into the merge means it survives rebases cleanly.

## Building and Installing

### The Rust toolchain gotcha

The default `stable` toolchain may be too old. The dependency tree needs **1.95 or newer**, because `monty` (the Python sandbox behind `code_execution`) requires 1.95 and the `ruff_*` crates require 1.93. A stale stable fails immediately with:

```
rustc 1.91.1 is not supported by the following packages:
  monty@0.0.18 requires rustc 1.95
```

Note that the root `Cargo.toml` still advertises `rust-version = "1.88"`, which is not what the tree actually builds with. `flake.nix` pins `1.95.0` and is the honest answer.

Either run `rustup update stable`, or pin a known-good toolchain per command:

```bash
cargo +1.97.1 build --release --locked --target-dir target
```

### Build and install

```bash
cargo +1.97.1 install --path . --locked --target-dir target
```

`--target-dir target` matters. Without it, `cargo install` builds in a throwaway temp directory, so the work is discarded and the next `cargo build` starts cold. Pointing it at the repo's own `target/` keeps the artifacts for development.

The binary lands in `~/.cargo/bin/maki`. Verify with:

```bash
maki --version
# maki 0.4.5 (4509b3d6)
```

The hash in parentheses is the commit the build came from, which is the only way to tell two builds of the same version apart. It comes from `build.rs`, and falls back to `unknown` when git is unavailable.

One wrinkle from jj colocation. `build.rs` watches `.git/HEAD` and `.git/refs/heads`, and jj only writes those when a bookmark moves or you `git checkout`. Working on an unbookmarked jj commit means the embedded hash still points at the last exported commit. It catches up as soon as the bookmark moves, and a released build is always correct because `cargo install` recompiles.

Maki is a TUI, so do not launch it bare when you only want to check the build. Use `--version` or `--help`.

### Full CI check

The `justfile` defines `just ci` as `fmt-check lint pylint test gen-docs-check machete`. If `just` or `cargo-nextest` are not installed, run the pieces directly:

```bash
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 clippy --all --tests --target-dir target -- -D warnings
cargo +1.97.1 test --workspace --target-dir target
cargo +1.97.1 run -p maki-docgen --target-dir target -- --check
```

Run `gen-docs-check` whenever you touch the provider inventory or manifest. `maki-docgen` reads `all_builtins()` and regenerates `site/docs/`, and CI fails on drift.

## Provider Auth Notes

Handy while working on provider branches.

Key resolution order, from `KeyPool::resolve`:

1. `<SLUG>_API_KEY` environment variable
2. `~/.local/state/maki/auth/<slug>.json` (written by `maki auth login <slug>`)
3. `api_key` under `[<slug>]` in `~/.config/maki/providers.toml`

Maki also reads `.env` files at startup, global `~/.config/maki/.env` first and then project `.maki/.env`, and neither overwrites a variable that is already set. Note that `load_env_files` runs for the TUI, ACP, `index` and `prompt`, but not for `maki models` or `maki auth status`, so those two subcommands will not see a key that only lives in a `.env` file.

Multiple keys can be comma separated in one variable and they rotate on rate-limit or auth errors.

## Common jj Commands

```bash
jj log                              # commit graph
jj status                           # current state
jj diff                             # working copy changes
jj bookmark list --all-remotes      # bookmarks and their remotes
jj bookmark create <name>           # create at current commit
jj bookmark set <name> -r <rev>     # move bookmark
jj new <rev>                        # new commit on top of rev
jj describe -m "message"            # change commit message
jj squash -u                        # move changes into parent, keep its message
jj rebase -s <rev> -d <dest>        # rebase rev and descendants
jj undo                             # undo last operation
jj op log                           # operation history
```

## File Locations

| File | Purpose |
|------|---------|
| `Cargo.toml` | Workspace manifest and version |
| `build.rs` | Captures the short git hash at build time as `GIT_HASH` |
| `src/cli.rs` | Version string, appends the hash |
| `FORK_WORKFLOW.md` | This documentation (on the fork-customizations branch) |
| `maki-providers/src/providers/` | One file per provider, each with its `inventory::submit!` |
| `maki-config/src/providers.rs` | `BuiltInProvider` struct and `providers.toml` handling |
| `justfile` | Build, lint, test and docs commands |
| `flake.nix` | Nix dev shell, pins the real Rust version |
| `target/release/maki` | Built executable |

## Rebase History

| Date | Upstream Range | New Commits | Conflicts | Notes |
|------|---------------|:-----------:|-----------|-------|
| 2026-08-07 | — → 91852e22 | — | — | Fork created; openrouter-auth, effort-levels and modal-hints branches added |

---

*Last updated: 2026-08-07*
*Fork created at upstream 91852e22; OpenRouter registered as a built-in provider, per-model effort levels and provider routing added*
