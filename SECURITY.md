# Security

Report vulnerabilities privately through GitHub:
https://github.com/PlayKigai/godot-bridge/security/advisories/new

Do not open a public issue for a security problem.

Include the bridge version (`godot-bridge --version`), the Godot version,
the OS, the editor client, and steps to reproduce.

Only the latest release is supported. Fixes are best effort by a single
maintainer, with no response time commitment. Reporters are credited in
the release notes unless they ask not to be.

Release binaries and the VSIX carry a `SHA256SUMS` file and a GitHub
build provenance attestation. Verify with:

    sha256sum -c SHA256SUMS
    gh attestation verify godot-bridge-*.tar.gz --repo PlayKigai/godot-bridge
