# Security policy

## Supported versions

Security fixes go into the latest release of the 0.1 series. Older releases are not patched; upgrade to the latest one.

## Reporting a vulnerability

Please report a vulnerability privately. Do not open a public issue or pull request for it.

Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**.

A useful report has:

- the version (`rradius-server --version`) and how it was built or installed;
- the relevant part of the configuration, with secrets removed;
- the steps or the packets that reproduce the problem, and what you expected instead;
- what an attacker gains, if you have worked that out.

## What happens next

We read each report, try to reproduce it, and reply on the report. A confirmed problem is fixed in a new release and described in [`CHANGELOG.md`](CHANGELOG.md); the report is made public together with the fix. This is a small project, and no response time is promised.

## Scope

This repository holds the open-source server, `rradius-bench` and the examples. Things to know before reporting:

- The example configurations and example secrets (`*.secret` files) are public sample values. A weak secret in an example is not a vulnerability; the server warns about it at start-up.
- The metrics port has no authentication. The manual says to bind it to an internal address and to protect it with a firewall.
