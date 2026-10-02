# Security policy

Please report vulnerabilities privately, using GitHub's "Report a vulnerability"
(private security advisories) on the affected repository. Do not open a public
issue for a security problem.

Include the affected component (`pos-backend`, `pos-frontend`, `document-server`
or the desktop shell), the version, and steps to reproduce.

Never commit secrets. Generate your own `JWT_SECRET` and `*_API_KEY` values
(`openssl rand -hex 32`). The backend refuses to start with the placeholder
`JWT_SECRET` or `DOCUMENT_SERVER_API_KEY` from the `.env` templates, and the
document server refuses an `INTERNAL_API_KEY` that is a placeholder or shorter
than 16 characters.

Desktop builds are signed for the in-app updater with a key held only in this
repository's Actions secrets; installed apps accept only updates signed with it.
