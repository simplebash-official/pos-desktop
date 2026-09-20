# Security policy

Please report vulnerabilities privately, using GitHub's "Report a vulnerability"
(private security advisories) on the affected repository. Do not open a public
issue for a security problem.

Include the affected component (`pos-backend`, `pos-frontend`, `document-server`
or the desktop shell), the version, and steps to reproduce.

Never commit secrets. Generate your own `JWT_SECRET` and `*_API_KEY` values
(`openssl rand -hex 32`); the backend refuses to start with the placeholder
value from the `.env` templates.
