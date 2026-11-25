ALTER TABLE pages
    ADD COLUMN legal_review_status text NOT NULL DEFAULT 'draft' CHECK (legal_review_status IN ('draft', 'reviewed')),
    ADD COLUMN reviewed_at timestamptz,
    ADD COLUMN reviewed_by uuid REFERENCES users (id) ON DELETE SET NULL;
