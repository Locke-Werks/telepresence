# Releasing

Releases are built by GitHub Actions from a version tag.

1. Bump `version` under `[workspace.package]` in `Cargo.toml`. The exe's
   version resource comes from there; CI warns if the tag disagrees.
2. Commit, then tag and push:

   ```
   git tag v0.3.0
   git push origin v0.3.0
   ```

3. `release.yml` runs the three platform workflows in parallel, then
   publishes one GitHub release with every artifact plus `SHA256SUMS`.

Tags must match `v*.*.*`. Each platform workflow can also be run by hand
(`workflow_dispatch`) to produce artifacts without publishing.

## Platform jobs

| Workflow | Runner | Produces |
| --- | --- | --- |
| `release-windows.yml` | `windows-latest` | `telepresence-<v>-windows-x64-setup.exe` |
| `release-macos.yml` | `macos-26` | `telepresence-<v>-macos-universal.dmg` and `.zip` |
| `release-linux.yml` | Ubuntu 24.04 | `telepresence_<v>_amd64.deb`, `telepresence-<v>-linux-x86_64.AppImage` |

Pinned versions live in each workflow's `env` block: Qt 6.11.3, GStreamer
1.26.x, Forge v0.4.1. The macOS job pins GStreamer package hashes. Keep
`FORGE_VERSION` in `release-windows.yml` and `$ForgeVersion` in
`packaging/windows/package.ps1` in step.

## Signing

Windows: Azure Trusted Signing. Our binaries are signed before staging (so
the payload Forge extracts is already signed), then the forged installer is
signed. Repository secrets:

- `AZURE_TENANT_ID`
- `AZURE_CLIENT_ID`
- `AZURE_CLIENT_SECRET`

macOS: Developer ID Application certificate, hardened runtime, secure
timestamp, notarized with an App Store Connect API key and stapled.
Repository secrets:

- `DEVELOPER_ID_APP_P12_BASE64`, `DEVELOPER_ID_APP_P12_PASSWORD`
- `ASC_KEY_P8_BASE64`, `ASC_KEY_ID`, `ASC_ISSUER_ID`

Linux packages are not signed.

## Updater contract

The in-app updater reads `releases/latest`, ignores prereleases, picks the
asset for the running platform by name (`update::asset_name`), and refuses
any asset whose hash is not in `SHA256SUMS`. Renaming an artifact in a
workflow or packaging script without changing `asset_name` breaks updates
for that platform.
