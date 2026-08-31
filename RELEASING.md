# Releasing Piwaku

Piwaku currently publishes macOS installers through GitHub Releases. The app
does not contain an in-app updater, Sparkle, appcast, or Cloudflare R2 sync.
Users download a DMG or ZIP from the repository's Releases page and update by
installing the new version themselves.

## Local prerequisites

- macOS 13 or newer
- Rust 1.96 or newer
- Bun
- `create-dmg` (`brew install create-dmg`)
- A Developer ID Application certificate and notarization credentials for a
  public release

The package script is:

```sh
bun run release
```

It builds the app bundle, creates a DMG and ZIP under `dist/`, verifies the
mounted DMG contents, and writes `dist/Piwaku-<version>.md` from the matching
section in `CHANGELOG.md`. It does not upload anything outside the local
workspace.

For a local ad-hoc package when signing credentials are unavailable:

```sh
bun run release --adhoc --local
```

The resulting package is suitable for local testing and is not notarized.

## GitHub Actions release

`.github/workflows/release.yml` builds only macOS artifacts. It accepts a `v*`
tag push or a manual workflow run, then opens a draft GitHub Release with:

- `Piwaku-<version>.dmg`
- `Piwaku-<version>.zip`
- `Piwaku-<version>.md`

The version in `Cargo.toml` must match the tag. Add the matching release notes
section to `CHANGELOG.md` before building.

The macOS job expects these repository Actions secrets:

| Secret | Purpose |
| --- | --- |
| `APPLE_CERTIFICATE` | Base64-encoded Developer ID certificate |
| `APPLE_CERTIFICATE_PASSWORD` | Certificate password |
| `APPLE_ID` | Apple ID for notarization |
| `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password |
| `APPLE_TEAM_ID` | Apple Developer Team ID |
| `WAKU_SIGNING_IDENTITY` | Developer ID Application identity |
| `WAKU_ANALYTICS_ENDPOINT` | Build-time analytics endpoint; required when CI builds binaries |
| `WAKU_ANALYTICS_WEBSITE_ID` | Build-time analytics site identifier; required when CI builds binaries |

The release script requires both variables when it builds binaries. For local
packaging with already-built binaries, pass `--skip-build` instead.

## Release checklist

1. Bump `version` in `Cargo.toml`.
2. Add a `## <version>` section to `CHANGELOG.md`.
3. Run the focused checks and package locally.
4. Commit and push the version bump to `main`.
5. Push the matching tag, for example `git push origin v0.1.17`.
6. Review the generated draft and publish it from GitHub.

There is no separate release website, update feed, R2 bucket, or in-app update
step.
