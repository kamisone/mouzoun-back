-- 0026_legacy_hero_shape: heroes created before the page builder (0023 renamed the old blocks but
-- kept their hero data as it was) still carry the old keys, which the hero schema refuses, so the
-- page cannot be published:
--   settings.layout ('full' = whole screen)  → settings.height = 'screen'
--   content.cta (button text, per language)  → content.cta_primary_label, and a primary button
--                                              (settings.cta_primary) leading to the stays page.
-- Rows already in the new shape are left alone.

UPDATE content_blocks b
SET settings = (b.settings - 'layout')
    || CASE WHEN b.settings->>'layout' = 'full' AND NOT b.settings ? 'height'
            THEN jsonb_build_object('height', 'screen') ELSE '{}'::jsonb END
    || CASE WHEN NOT b.settings ? 'cta_primary'
                 AND EXISTS (SELECT 1 FROM content_block_translations t
                             WHERE t.content_block_id = b.id AND t.content ? 'cta')
            THEN jsonb_build_object('cta_primary', jsonb_build_object(
                     'link', jsonb_build_object('type', 'route', 'route', '/stays', 'new_tab', false),
                     'style', 'primary'))
            ELSE '{}'::jsonb END,
    updated_at = now()
WHERE b.block_type = 'hero'
  AND (b.settings ? 'layout'
       OR EXISTS (SELECT 1 FROM content_block_translations t
                  WHERE t.content_block_id = b.id AND t.content ? 'cta'));

UPDATE content_block_translations t
SET content = (t.content - 'cta')
    || CASE WHEN NOT t.content ? 'cta_primary_label' AND coalesce(t.content->>'cta', '') <> ''
            THEN jsonb_build_object('cta_primary_label', left(t.content->>'cta', 60))
            ELSE '{}'::jsonb END,
    updated_at = now()
FROM content_blocks b
WHERE b.id = t.content_block_id
  AND b.block_type = 'hero'
  AND t.content ? 'cta';
