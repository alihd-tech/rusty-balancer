# Security policy

Rusty Balancer is an experimental pre-release. There is no supported production release yet. Do not expose it directly to untrusted traffic without independent testing and review.

Report vulnerabilities privately using GitHub's private vulnerability reporting if enabled, or contact a repository maintainer privately. Do not publish exploit details, credentials, private keys or live infrastructure configuration in public issues. Include the affected commit/version, reproduction steps and impact. No response-time SLA is promised.

Security assumptions: trusted local configuration and administrators; static roots containing only intended public files; trusted HTTP upstreams; restrictive ACLs on UDP; TLS passthrough proxies do not authenticate upstream application identity. Consult docs/security.md for deployment and validation limitations.
