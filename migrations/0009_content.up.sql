-- 0009_content: reviews, pages + page builder blocks, navigation, FAQs, banners, redirects.

CREATE TABLE reviews (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id      uuid REFERENCES products (id) ON DELETE CASCADE,     -- NULL = review of the whole camp
    booking_id      uuid REFERENCES bookings (id) ON DELETE SET NULL,
    customer_id     uuid REFERENCES customers (id) ON DELETE SET NULL,
    author_name     text NOT NULL,
    author_country  char(2) CHECK (author_country ~ '^[A-Z]{2}$'),
    locale          text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    rating          smallint NOT NULL CHECK (rating BETWEEN 1 AND 5),
    sub_ratings     jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(sub_ratings) = 'object'),
    title           text,
    body            text,
    experience_date date,
    source          text NOT NULL DEFAULT 'website' CHECK (source IN ('website', 'google', 'tripadvisor',
                        'booking_com', 'airbnb', 'manual')),
    status          text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected', 'spam')),
    is_featured     boolean NOT NULL DEFAULT false,
    reply_body      text,
    replied_at      timestamptz,
    replied_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX reviews_product_idx ON reviews (product_id, status);
CREATE INDEX reviews_status_idx ON reviews (status, created_at DESC);

CREATE TABLE pages (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    page_type      text NOT NULL DEFAULT 'page' CHECK (page_type IN ('home', 'page', 'post', 'legal', 'landing')),
    template       text NOT NULL DEFAULT 'default',
    parent_id      uuid REFERENCES pages (id) ON DELETE SET NULL,
    category_id    uuid REFERENCES categories (id) ON DELETE SET NULL,
    status         text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'scheduled', 'published', 'archived')),
    publish_at     timestamptz,
    author_id      uuid REFERENCES users (id) ON DELETE SET NULL,
    cover_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    in_sitemap     boolean NOT NULL DEFAULT true,
    sort_order     integer NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz,
    CHECK (parent_id <> id),
    CHECK (status <> 'scheduled' OR publish_at IS NOT NULL)
);
-- At most one live home page.
CREATE UNIQUE INDEX pages_single_live_home ON pages (page_type)
    WHERE page_type = 'home' AND status = 'published' AND deleted_at IS NULL;
CREATE INDEX pages_status_idx ON pages (status, page_type) WHERE deleted_at IS NULL;

CREATE TABLE page_translations (
    page_id         uuid NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    locale          text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    title           text NOT NULL,
    slug            text NOT NULL CHECK (slug ~ '^[^\s?#]+$' AND slug !~ '(^/|/$|//)'),   -- may contain '/'
    excerpt         text,
    seo_title       text,
    seo_description text,
    og_media_id     uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    canonical_url   text,
    noindex         boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, locale),
    UNIQUE (locale, slug)
);

-- Page builder: an ordered list of typed blocks attached to exactly one page, product or place.
CREATE TABLE content_blocks (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    page_id    uuid REFERENCES pages (id) ON DELETE CASCADE,
    product_id uuid REFERENCES products (id) ON DELETE CASCADE,
    place_id   uuid REFERENCES places (id) ON DELETE CASCADE,
    block_type text NOT NULL CHECK (block_type ~ '^[a-z][a-z0-9_]*$'),
    settings   jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings) = 'object'),   -- non-translatable
    sort_order integer NOT NULL DEFAULT 0,
    is_visible boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(page_id, product_id, place_id) = 1)
);
CREATE INDEX content_blocks_page_idx ON content_blocks (page_id, sort_order) WHERE page_id IS NOT NULL;
CREATE INDEX content_blocks_product_idx ON content_blocks (product_id, sort_order) WHERE product_id IS NOT NULL;
CREATE INDEX content_blocks_place_idx ON content_blocks (place_id, sort_order) WHERE place_id IS NOT NULL;

CREATE TABLE content_block_translations (
    content_block_id uuid NOT NULL REFERENCES content_blocks (id) ON DELETE CASCADE,
    locale           text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    content          jsonb NOT NULL DEFAULT '{}',
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (content_block_id, locale)
);

CREATE TABLE nav_menus (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,     -- header / footer / mobile
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE nav_menu_items (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    menu_id         uuid NOT NULL REFERENCES nav_menus (id) ON DELETE CASCADE,
    parent_id       uuid REFERENCES nav_menu_items (id) ON DELETE CASCADE,
    link_type       text NOT NULL CHECK (link_type IN ('page', 'product', 'category', 'place', 'url', 'route')),
    page_id         uuid REFERENCES pages (id) ON DELETE CASCADE,
    product_id      uuid REFERENCES products (id) ON DELETE CASCADE,
    category_id     uuid REFERENCES categories (id) ON DELETE CASCADE,
    place_id        uuid REFERENCES places (id) ON DELETE CASCADE,
    url             text,
    route           text,                  -- named front-end route, e.g. "activities"
    icon            text,
    open_in_new_tab boolean NOT NULL DEFAULT false,
    is_visible      boolean NOT NULL DEFAULT true,
    sort_order      integer NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    -- exactly the target column matching link_type is set
    CHECK (num_nonnulls(page_id, product_id, category_id, place_id, url, route) = 1),
    CHECK (CASE link_type
               WHEN 'page'     THEN page_id IS NOT NULL
               WHEN 'product'  THEN product_id IS NOT NULL
               WHEN 'category' THEN category_id IS NOT NULL
               WHEN 'place'    THEN place_id IS NOT NULL
               WHEN 'url'      THEN url IS NOT NULL
               WHEN 'route'    THEN route IS NOT NULL
           END),
    CHECK (parent_id <> id)
);
CREATE INDEX nav_menu_items_menu_idx ON nav_menu_items (menu_id, parent_id, sort_order);

CREATE TABLE nav_menu_item_translations (
    nav_menu_item_id uuid NOT NULL REFERENCES nav_menu_items (id) ON DELETE CASCADE,
    locale           text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    label            text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (nav_menu_item_id, locale)
);

CREATE TABLE faqs (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    category_id  uuid REFERENCES categories (id) ON DELETE SET NULL,
    product_id   uuid REFERENCES products (id) ON DELETE CASCADE,
    sort_order   integer NOT NULL DEFAULT 0,
    is_published boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE faq_translations (
    faq_id     uuid NOT NULL REFERENCES faqs (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    question   text NOT NULL,
    answer     text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (faq_id, locale)
);

CREATE TABLE banners (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    placement     text NOT NULL CHECK (placement IN ('top_bar', 'popup', 'home_hero', 'booking_funnel')),
    style         text,
    link_url      text,
    media_id      uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    active_period tstzrange,
    is_active     boolean NOT NULL DEFAULT true,
    sort_order    integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE banner_translations (
    banner_id  uuid NOT NULL REFERENCES banners (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    message    text NOT NULL,
    cta_label  text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (banner_id, locale)
);

CREATE TABLE redirects (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    from_path   text NOT NULL UNIQUE CHECK (from_path ~ '^/'),
    to_path     text NOT NULL,
    status_code smallint NOT NULL DEFAULT 301 CHECK (status_code IN (301, 302, 307, 308)),
    hits        bigint NOT NULL DEFAULT 0,
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (from_path <> to_path)
);
