# moa-submitter

Submit kernels to the MOA 2026 kernel optimization competition.

## Install

```sh
cargo binstall moa-submitter-cli
```

## Use

```sh
moa-submitter login              # once; the token lasts 30 days
cd furiosa-opt-gemma4-12B        # your clone of the baseline
moa-submitter submit
moa-submitter status
```

`submit` finds the repository by walking up from the current directory, so it works from
anywhere inside your clone.

| Command | |
| --- | --- |
| `moa-submitter status` | your 20 most recent submissions |
| `moa-submitter status -n 50` | the 50 most recent instead |
| `moa-submitter status --all` | every submission you have made |
| `moa-submitter status <ID>` | per-kernel cycles, score, and why it failed |
| `moa-submitter log <ID>` | that submission's log, split by stage |

## What gets uploaded

`src/ops.rs` and everything under `src/device/`.
