# Install prebuilt binaries

Each tagged release ships prebuilt `ghm` and `gwu-hpc-mcp` binaries, so you do
not need Rust or Xcode's command-line tools to install them. Releases are built
by [dist](https://github.com/axodotdev/cargo-dist) in GitHub Actions; see
[Cutting a release](#cutting-a-release).

After installing, continue with the
[first connection guide](../README.md#first-connection-from-config-to-ghm-up).
Run `ghm --version` to confirm which binary is on your PATH.

## What a release contains

| Platform | Archive | Status |
| --- | --- | --- |
| macOS, Apple silicon | `gwu-hpc-mcp-aarch64-apple-darwin.tar.xz` | Supported |
| macOS, Intel | `gwu-hpc-mcp-x86_64-apple-darwin.tar.xz` | Supported |
| Linux / WSL, x86_64 | `gwu-hpc-mcp-x86_64-unknown-linux-musl.tar.xz` | Supported |
| Linux / WSL, ARM64 | `gwu-hpc-mcp-aarch64-unknown-linux-musl.tar.xz` | Supported |
| Windows, x86_64 and ARM64 | `gwu-hpc-mcp-x86_64-pc-windows-msvc.zip` | Supported |

The Linux builds are statically linked against musl, so one binary runs on any
distribution and inside WSL with no glibc version requirements.

Each archive has a matching `.sha256` file, and `sha256.sum` lists all of
them. The release also includes `gwu-hpc-mcp-installer.sh` (macOS, Linux, WSL)
and `gwu-hpc-mcp-installer.ps1` (Windows).

Not sure which archive you need? Run `uname -sm`:

- `Darwin arm64` → `aarch64-apple-darwin`
- `Darwin x86_64` → `x86_64-apple-darwin`
- `Linux x86_64` → `x86_64-unknown-linux-musl`
- `Linux aarch64` → `aarch64-unknown-linux-musl`

On Windows, use the `x86_64-pc-windows-msvc` build. It runs natively with
ghm's built-in SSH client, so it needs neither WSL nor OpenSSH, and Windows on
ARM runs it through its built-in x64 emulation. If you prefer WSL, install the
Linux build inside WSL instead; the two installations do not share a
connection.

## While the repository is private

The GitHub repository is private, so its release assets require GitHub
authentication. The standard one-liner installers download anonymously and
**will fail with a 404** until the assets are public:

```sh
# Does NOT work while the repository is private:
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/RileyLeff/gwu-hpc-mcp/releases/latest/download/gwu-hpc-mcp-installer.sh | sh
```

Use the [GitHub CLI](https://cli.github.com/) instead. Install it, run
`gh auth login` with an account that has access to the repository, then use
one of the options below. Omitting the tag downloads the latest release; pass
a tag such as `v0.1.3` to pin one.

### Option A: run the installer against downloaded files

This uses the release's own installer, which puts both binaries in
`~/.cargo/bin` (the same place `cargo install` used) and adds that directory to
your PATH. In a macOS, Linux, or WSL shell:

```sh
mkdir -p ~/ghm-download && cd ~/ghm-download
gh release download --repo RileyLeff/gwu-hpc-mcp --clobber \
  --pattern 'gwu-hpc-mcp-installer.sh' \
  --pattern 'gwu-hpc-mcp-*.tar.xz' \
  --pattern 'sha256.sum'
GWU_HPC_MCP_DOWNLOAD_URL="file://$PWD" sh ./gwu-hpc-mcp-installer.sh
```

In Windows PowerShell (install the GitHub CLI with `winget install GitHub.cli`
first):

```powershell
New-Item -ItemType Directory -Force "$HOME\ghm-download" | Out-Null
Set-Location "$HOME\ghm-download"
gh release download --repo RileyLeff/gwu-hpc-mcp --clobber `
  --pattern 'gwu-hpc-mcp-installer.ps1' `
  --pattern 'gwu-hpc-mcp-x86_64-pc-windows-msvc.zip' `
  --pattern 'sha256.sum'
$env:GWU_HPC_MCP_DOWNLOAD_URL = ([Uri]$PWD.Path).AbsoluteUri
powershell -ExecutionPolicy Bypass -File .\gwu-hpc-mcp-installer.ps1
```

`GWU_HPC_MCP_DOWNLOAD_URL` points the installer at the local files instead of
GitHub, so it never needs network access to the private repository. Open a new
terminal afterwards so the PATH change takes effect. If you previously
installed with `cargo install --path .`, the installer replaces that copy in
place. Set `GWU_HPC_MCP_NO_MODIFY_PATH=1` to skip editing your shell profile
(or, on Windows, your user PATH). On Windows, run `ghm down` and close your
agent before reinstalling; Windows cannot replace a running `.exe`.

### Option B: extract one archive by hand

Replace `TARGET` with your platform from the table above:

```sh
TARGET=aarch64-apple-darwin
mkdir -p ~/ghm-download && cd ~/ghm-download
gh release download --repo RileyLeff/gwu-hpc-mcp --clobber \
  --pattern "gwu-hpc-mcp-$TARGET.tar.xz*"
shasum -a 256 -c "gwu-hpc-mcp-$TARGET.tar.xz.sha256"   # Linux/WSL: sha256sum -c
tar -xJf "gwu-hpc-mcp-$TARGET.tar.xz"
mkdir -p ~/.cargo/bin
mv "gwu-hpc-mcp-$TARGET/ghm" "gwu-hpc-mcp-$TARGET/gwu-hpc-mcp" ~/.cargo/bin/
```

The checksum line should end in `OK`. On macOS, `shasum` may also print
`1 line is improperly formatted`; that is a trailing blank line in the
checksum file and is harmless. Make sure `~/.cargo/bin` is on your PATH. The
MCP registration commands in [client-setup.md](client-setup.md) reference
`$HOME/.cargo/bin/ghm`; if you install elsewhere, use that path instead.

On Windows, in PowerShell:

```powershell
$zip = 'gwu-hpc-mcp-x86_64-pc-windows-msvc.zip'
New-Item -ItemType Directory -Force "$HOME\ghm-download" | Out-Null
Set-Location "$HOME\ghm-download"
gh release download --repo RileyLeff/gwu-hpc-mcp --clobber --pattern "$zip*"
$expected = (Get-Content "$zip.sha256").Split(' ')[0]
if ((Get-FileHash $zip).Hash -ne $expected) { throw 'checksum mismatch' }
Expand-Archive -Force $zip extracted
$bin = "$HOME\.cargo\bin"
New-Item -ItemType Directory -Force $bin | Out-Null
Move-Item -Force extracted\*.exe $bin
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $bin) {
  [Environment]::SetEnvironmentVariable('Path', "$path;$bin", 'User')
}
```

This checks the archive, installs both `.exe` files in `%USERPROFILE%\.cargo\bin`,
and adds that folder to your user PATH. Open a new terminal afterwards.

### Making the binaries public

Two ways to let people install without GitHub access to the source:

1. **Make this repository public.** The one-liner installers above,
   `cargo binstall`, and a Homebrew tap then work with no extra setup.
2. **Publish releases to a separate public repository** while the source stays
   private. Create a public repository (for example
   `RileyLeff/gwu-hpc-mcp-releases`), add a fine-grained token with
   *Contents: read and write* on it as a `GH_RELEASES_TOKEN` secret in this
   repository, then add to `dist-workspace.toml`:

   ```toml
   [dist]
   github-releases-repo = "RileyLeff/gwu-hpc-mcp-releases"
   ```

   Run `dist init` to regenerate the workflow. The installers then download
   from the public repository. Anyone can download the binaries, so only do
   this if distributing them openly is acceptable.

The installer also reads `GWU_HPC_MCP_GITHUB_TOKEN` and sends it as a bearer
token. This has not been tested against private-repository release URLs; use
the `gh` options above rather than relying on it.

## Once releases are public

macOS, Linux, or WSL:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/RileyLeff/gwu-hpc-mcp/releases/latest/download/gwu-hpc-mcp-installer.sh | sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/RileyLeff/gwu-hpc-mcp/releases/latest/download/gwu-hpc-mcp-installer.ps1 | iex"
```

Rust users can also skip compiling with
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall). The crate is
not on crates.io, so point it at the repository:

```sh
cargo binstall --git https://github.com/RileyLeff/gwu-hpc-mcp gwu-hpc-mcp
```

The binstall metadata in `Cargo.toml` maps each target to its release archive.

## macOS Gatekeeper

The macOS binaries are not signed with an Apple Developer ID or notarized.
Whether Gatekeeper objects depends on how you download them:

- **Terminal downloads are fine.** The `curl` installer, `gh release
  download`, and `tar` in Terminal do not add the quarantine attribute, so the
  binaries run without a prompt.
- **Browser downloads are blocked.** Safari and Chrome mark downloads as
  quarantined, and macOS then refuses to run `ghm` ("Apple could not verify
  ..."). Clear the attribute after extracting:

  ```sh
  xattr -d com.apple.quarantine ~/.cargo/bin/ghm ~/.cargo/bin/gwu-hpc-mcp
  ```

Removing the need for that step requires notarization, which needs a paid
Apple Developer account. dist can codesign macOS binaries with a Developer ID
certificate (the `macos-sign` setting), but it does not notarize yet, so that
would take a custom `notarytool` step in the release workflow.

## Windows SmartScreen and signing

The Windows build is unsigned. Unsigned `.exe` files
downloaded with a browser trigger SmartScreen ("Windows protected your PC");
users must choose **More info → Run anyway**. The PowerShell installer
downloads with `irm`, which does not mark files as downloaded from the
internet, so it does not trigger SmartScreen.

The planned signing path is
[Azure Artifact Signing](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart)
(formerly Trusted Signing), which dist 0.33 supports directly. Signing signs
the `.exe` files and the PowerShell installer during the release. Check
Microsoft's current eligibility and pricing first. To enable it:

1. Create an Artifact Signing account, complete identity validation, and
   create a Public Trust certificate profile.
2. Create an Azure app registration with a GitHub OIDC federated credential
   for `repo:RileyLeff/gwu-hpc-mcp:environment:release`, and give it the
   *Artifact Signing Certificate Profile Signer* role.
3. Add `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID` as
   repository secrets.
4. Add to `dist-workspace.toml`, then run `dist init`:

   ```toml
   [dist.azure-windows-sign]
   endpoint = "https://REGION.codesigning.azure.net/"
   account-name = "YOUR_SIGNING_ACCOUNT"
   certificate-profile-name = "YOUR_CERTIFICATE_PROFILE"
   ```

Until then, prefer the PowerShell installer or `gh release download` over a
browser download.

## Cutting a release

Release configuration lives in `dist-workspace.toml`; the workflow in
`.github/workflows/release.yml` is generated from it. After editing the config,
run `dist init` (or `dist generate`) to regenerate the workflow rather than
editing it by hand. Install dist with `brew install cargo-dist` or from its
[releases](https://github.com/axodotdev/cargo-dist/releases).

1. Bump `version` in `Cargo.toml` and commit.
2. Check locally:

   ```sh
   dist plan
   dist build --artifacts=all --target aarch64-apple-darwin
   ```

   Pass your own host's target. Without `--target`, dist tries every
   platform and needs `cargo-zigbuild` and `cargo-xwin` to cross-compile.
   Output lands in `target/distrib/`.

3. Tag and push. The tag must match the `Cargo.toml` version:

   ```sh
   git tag v0.1.4
   git push upstream v0.1.4
   ```

The workflow builds every target, then creates a GitHub Release with the
archives, checksums, and installers. A version with a suffix such as
`v0.2.0-rc.1` is published as a prerelease. Pull requests run only `dist plan`.

Notes for the private repository:

- Actions minutes on macOS runners are billed at a higher rate than Linux, and
  each release runs two macOS jobs.
- The aarch64 Linux build runs on GitHub's `ubuntu-22.04-arm` runner. If that
  job cannot get a runner on this repository, cross-compile it from an x86_64
  runner instead by adding the following to `dist-workspace.toml` and running
  `dist init`:

  ```toml
  [dist.github-custom-runners]
  aarch64-unknown-linux-musl = "ubuntu-22.04"
  ```

### Homebrew (not enabled)

`dist-workspace.toml` contains a commented-out Homebrew tap configuration.
Enabling it needs a public tap repository (for example
`RileyLeff/homebrew-tap`), a `HOMEBREW_TAP_TOKEN` secret with write access to
it, and publicly downloadable release assets. Then users could run
`brew install RileyLeff/tap/gwu-hpc-mcp`.
