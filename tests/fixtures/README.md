The RSA key pair in this directory is a public, test-only fixture. It is never
registered with GitHub or used for production authentication. Tests copy the
private key into a temporary file with mode 0600 and verify JWT signatures using
the matching PKCS#1 DER public key.
