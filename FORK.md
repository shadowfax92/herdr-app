# Herdr Fork

One rolling `fork` branch on `shadowfax92/herdr-app`, rebased onto upstream `v*` tags and shipped as a self-signed macOS bundle (`Herdr Fork.app`, id `dev.herdr.app.fork`, updater disabled).

```text
upstream v* tag ──► fork-sync (6h cron, ubuntu-22.04)
                         │
            ┌────────────┴─────────────┐
            ▼ clean + tests green      ▼ conflict / test fail
     backup/pre-<tag> + push fork   open/update `fork-sync` issue
     tag fork-v<ver>-<n>            fix locally: fork/scripts/fork-rebase
            │
            ▼ dispatch fork-release (release env, self-signed)
     GitHub Release (tar.gz + SHA256SUMS)
            │
            ▼ on Mac
     fork/scripts/fork-pull ──► ~/Applications/Herdr Fork.app
```

## Commands

- `fork/scripts/fork-pull [--tag <t>] [--dest <dir>]`: download, verify `SHA256SUMS` + `codesign`, check all `~/.config/herdr-app/mods/shadowfax*` mods, swap `~/Applications/Herdr Fork.app` (backup in `~/.local/share/herdr-fork-backup/`), update `~/.local/bin/herdr-app`, and run `hc update` only if `hc` is below `distribution/core-version`.
- `fork/scripts/fork-rollback`: swap the previous bundle back from `~/.local/share/herdr-fork-backup/`.
- `fork/scripts/fork-rebase [target]`: local rebase onto latest upstream `v*` tag (or ref) with backup ref, `git rerere`, `--autosquash`, `range-diff`, and `cargo test --locked`.

## Conventions

- One commit per feature, new code in new files + thin hooks; never touch `Cargo.toml` version.
- Tags: `fork-v<upstream>-<n>` (starting `fork-v0.1.18-1`); backups: `backup/pre-<tag>`.
- Never build with `HERDR_APP_RELEASE`. Upstream `ci.yml` and `release.yml` stay disabled via `gh workflow disable`.
- Signing cert lives outside git in `~/.local/share/herdr-fork-signing/` and in GitHub Environment `release` (`fork-v*` tags only).
