# Security policy

BFS-NG is **pre-release research software**. There is no supported production filesystem, on-disk compatibility guarantee, or security-update SLA.

Do not mount or store irreplaceable data using experimental BFS-NG code. Run tests on disposable images only.

For non-sensitive correctness bugs, file a public GitHub issue. For vulnerabilities, use GitHub's private vulnerability reporting feature if enabled on this repository; otherwise contact the repository owner privately rather than publishing exploit details or credentials.

Never include real file paths, credentials, personal data or secrets in submitted cache traces. Prefer per-run salted opaque keys.
