# Security

Report vulnerabilities privately through GitHub:
https://github.com/PlayKigai/godot-bridge/security/advisories/new

Do not open a public issue for a security problem.

Include the bridge version (`godot-bridge --version`), the Godot version,
the OS, the editor client, and steps to reproduce.

Only the latest release is supported. Fixes are best effort by a single
maintainer, with no response time commitment.

Each release publishes a `SHA256SUMS` file next to the bare bridge binaries
and the VSIX, and every file carries a GitHub build provenance attestation.
Bridge assets are named `godot-bridge-vX.Y.Z-<arch>-<os>`
(`godot-bridge-v1.0.1-x86_64-linux`,
`godot-bridge-v1.0.1-aarch64-linux`,
`godot-bridge-v1.0.1-x86_64-windows.exe`,
`godot-bridge-v1.0.1-aarch64-windows.exe`); there are no archives.

Download the files you want plus `SHA256SUMS` into one directory. Run all
three steps in this order; the checksum file is only trustworthy once its own
attestation has been checked. Substitute your version for `v1.0.1`.

    gh attestation verify SHA256SUMS --repo PlayKigai/godot-bridge \
      --signer-workflow PlayKigai/godot-bridge/.github/workflows/release.yml \
      --source-ref refs/tags/v1.0.1

    sha256sum -c --ignore-missing SHA256SUMS

    gh attestation verify godot-bridge-v1.0.1-x86_64-linux --repo PlayKigai/godot-bridge \
      --signer-workflow PlayKigai/godot-bridge/.github/workflows/release.yml \
      --source-ref refs/tags/v1.0.1

Repeat the last command for each file you downloaded, swapping the filename:
`godot-bridge-v1.0.1-x86_64-windows.exe` or `godot-bridge-v1.0.1.vsix`. The
two `--signer-*` flags are what tie the file to this repository's release
workflow at that exact tag; without them any attestation from the repository
is accepted.

On Windows without `sha256sum`: `Get-FileHash <file> -Algorithm SHA256` and
compare with the line in `SHA256SUMS`.

Clients download the bridge and check its SHA-256 against the `SHA256SUMS`
of the release tagged with the client version. They trust TLS to github.com
and do not verify the Sigstore attestation. A proxy that terminates TLS can
substitute both files consistently. To check provenance for a
client-downloaded file, run the commands above against the stored file.
Storage locations: VS Code keeps it in the extension's global storage folder
(e.g. `~/.config/Code/User/globalStorage/PlayKigai.godot-bridge` on Linux);
Zed keeps it in the extension work dir
(`~/.local/share/zed/extensions/work/godot` on Linux); Neovim keeps it under
`<stdpath("data")>/godot-bridge` (run `:echo stdpath("data")` to see the
directory).
