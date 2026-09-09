# Security

Report vulnerabilities privately through GitHub:
https://github.com/PlayKigai/godot-bridge/security/advisories/new

Do not open a public issue for a security problem.

Include the bridge version (`godot-bridge --version`), the Godot version,
the OS, the editor client, and steps to reproduce.

Only the latest release is supported. Fixes are best effort by a single
maintainer, with no response time commitment.

Each release publishes a `SHA256SUMS` file next to the binaries and the
VSIX, and every file carries a GitHub build provenance attestation.
Download the files you need plus `SHA256SUMS` into one directory, then:

    sha256sum -c --ignore-missing SHA256SUMS
    gh attestation verify godot-bridge-vX.Y.Z-x86_64-linux.tar.gz --repo PlayKigai/godot-bridge
    gh attestation verify godot-bridge-vX.Y.Z-x86_64-windows.zip --repo PlayKigai/godot-bridge
    gh attestation verify godot-bridge-vX.Y.Z.vsix --repo PlayKigai/godot-bridge
    gh attestation verify SHA256SUMS --repo PlayKigai/godot-bridge

On Windows without `sha256sum`: `Get-FileHash <file> -Algorithm SHA256` and
compare with the line in `SHA256SUMS`.
