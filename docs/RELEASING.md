# Releasing remux

1. Bump `version` in `Cargo.toml` (and commit).
2. Tag and push:

   ```sh
   git tag v0.2.0 && git push origin main v0.2.0
   ```

3. The `release` workflow builds, natively per platform:
   - Linux tarballs + `.deb`s (x86_64, arm64 — arm via GitHub's arm runners),
   - macOS tarballs (arm64, x86_64),
   - `SHA256SUMS`, and `remux.rb` (the Homebrew formula with hashes filled in),

   and creates a **draft** GitHub release. Review it, then publish.

4. Homebrew: copy the generated `remux.rb` from the release assets into the
   tap repository (`jorgeamado/homebrew-remux`, path `Formula/remux.rb`).
   Create that repo once; after that this copy step is the whole "publish to
   brew" process (automatable later with a tap-update job + a PAT).

Notes:

- The tag must match `Cargo.toml`'s version — the workflow enforces this.
- Release binaries embed `web/dist` (rust-embed embeds in `--release`;
  debug builds read from disk), so CI builds the web client first.
- apt today means "install the .deb from GitHub releases". A hosted apt
  repository (e.g. a `deb [signed-by=…]` line served from GitHub Pages)
  is the follow-up if demand appears.
- Windows is documented as WSL2; there is no native build because tmux
  does not run natively on Windows.
