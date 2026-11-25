-- Demo data; delete this file in production.
-- The password column is the SHA-512 crypt hash of ops1-password (generated with openssl passwd -6).
-- JBSWY3DPEHPK3PXP is a common demo key; add it to an authenticator app to generate codes.
INSERT INTO mfa_users (username, password, totp_secret) VALUES ('ops1', '{crypt}$6$rradiusdemo$ieIZMv2GV9zanIl2gRuqVg1gQ6V8t41rAF2z4tPKeOpS3hw8kcJpsSxyTYM5VEaqDecWhZ0YG0yPYsQZtdBPi1', 'JBSWY3DPEHPK3PXP');
