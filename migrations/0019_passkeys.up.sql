-- 0019_passkeys: WebAuthn credentials (passkeys) of guest accounts. See docs/accounts/README.md.
-- Only ES256 keys are requested from browsers; the public key is kept as SubjectPublicKeyInfo DER.
CREATE TABLE webauthn_credentials (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    credential_id bytea NOT NULL UNIQUE,
    public_key    bytea NOT NULL,
    algorithm     integer NOT NULL DEFAULT -7 CHECK (algorithm = -7),
    sign_count    bigint NOT NULL DEFAULT 0 CHECK (sign_count >= 0),
    transports    text[] NOT NULL DEFAULT '{}',
    backed_up     boolean NOT NULL DEFAULT false,
    name          text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 80),
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz
);
CREATE INDEX webauthn_credentials_user_idx ON webauthn_credentials (user_id);
