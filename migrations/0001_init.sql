CREATE TABLE oauth_grants (
    issuer text PRIMARY KEY,
    tokens jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE images (
    id bigserial PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now(),
    provider text NOT NULL,
    model text NOT NULL,
    prompt text NOT NULL,
    revised_prompt text,
    settings jsonb NOT NULL,
    mime_type text NOT NULL,
    data bytea NOT NULL
);

CREATE INDEX images_created_at_idx ON images (created_at DESC);
