# Security policy

## Reporting a vulnerability

Please do not open a public issue for security-sensitive reports. Instead, use GitHub's private
[security advisory](https://github.com/BotCoder254/piku/security/advisories/new) form to disclose the
issue privately.

Include the affected version or commit, a description of the vulnerability, and, where possible, a
minimal reproduction. You can expect an acknowledgement and an initial assessment, and coordinated
disclosure once a fix is available.

## Scope

PIKU parses untrusted files (images, PDFs, archives, audio, and video). Reports about parser
crashes, resource exhaustion, path traversal, or any way to escape the sanitized storage boundary
are especially valuable.
