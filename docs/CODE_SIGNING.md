# Code Signing — Skeleton Notes (S09)

This document captures the placeholder configuration added in
[`src-tauri/tauri.conf.json`](../src-tauri/tauri.conf.json) for code
signing on Windows and macOS. **No actual signing is performed today.**
The fields are present so that signing can be enabled by populating
secrets without a schema change.

## Status

- **Windows (Authenticode)**: skeleton only — `certificateThumbprint`
  is `null`. No certificate purchased yet.
- **macOS**: skeleton only — `signingIdentity` and `entitlements` are
  `null`. No Apple Developer ID assigned.
- CI does **not** auto-sign. The build job in
  [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) only runs
  `cargo check / test / clippy`. Distribution builds happen locally.

## Tauri 1.x configuration in scope

```json
"bundle": {
  "windows": {
    "certificateThumbprint": null,
    "digestAlgorithm": "sha256",
    "timestampUrl": "http://timestamp.digicert.com",
    "tsp": false,
    "wix": null
  },
  "macOS": {
    "frameworks": null,
    "minimumSystemVersion": "10.15",
    "exceptionDomain": null,
    "signingIdentity": null,
    "providerShortName": null,
    "entitlements": null
  }
}
```

## When signing is enabled — Windows

1. Acquire an EV or OV code-signing certificate (DigiCert, Sectigo,
   GlobalSign).
2. Install the certificate into the Windows certificate store on the
   build machine (or use a hardware token / Azure Key Vault).
3. Replace `certificateThumbprint: null` with the SHA-1 thumbprint
   (e.g., `"A1B2C3D4..."`).
4. Verify the timestamp URL is reachable from the build environment.
   The DigiCert URL is the default; `http://timestamp.sectigo.com`
   is an alternative.
5. Build with `tauri build --release`. The resulting `.msi` / `.exe`
   should validate with `signtool verify /pa /v <path>`.

## When signing is enabled — macOS

1. Enroll in the Apple Developer Program ($99/yr).
2. Install a Developer ID Application certificate via Xcode or Apple
   Configurator.
3. Set `signingIdentity` to the certificate's common name, e.g.,
   `"Developer ID Application: Tony Example (TEAMID)"`.
4. Author an entitlements plist:
   ```xml
   <plist version="1.0"><dict>
     <key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
     <key>com.apple.security.cs.disable-library-validation</key><true/>
   </dict></plist>
   ```
   Place at `src-tauri/entitlements.plist` and reference it via
   `"entitlements": "entitlements.plist"`.
5. After signing, notarize:
   ```bash
   xcrun notarytool submit SentinelTrend.app.zip \
       --apple-id <appleid> --team-id <teamid> --password <app-password>
   xcrun stapler staple SentinelTrend.app
   ```
6. `hardenedRuntime` is enabled by default in our skeleton; this is
   required for notarization.

## CI integration (future)

When signing becomes mandatory, gate behind a manual workflow trigger
that loads secrets:

```yaml
name: release-signed
on:
  workflow_dispatch:
jobs:
  build-signed:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - name: Decode certificate
        run: |
          echo "$WINDOWS_CERT_BASE64" | base64 -d > cert.pfx
          # Import cert with WINDOWS_CERT_PASSWORD secret
      - name: Tauri build
        env:
          TAURI_PRIVATE_KEY: ${{ secrets.TAURI_PRIVATE_KEY }}
        run: cargo tauri build --release
```

Until signing is in place, distribute the unsigned binary with a
SHA-256 checksum and clear instructions in the README so end users can
self-verify integrity.

## References

- Tauri 1.x signing docs: <https://tauri.app/v1/guides/distribution/sign-windows>
- Apple notarization: <https://developer.apple.com/documentation/security/notarizing_macos_software_before_distribution>
