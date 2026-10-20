-- Run on every boot. raw_sql executes this as one implicit transaction, so the
-- advisory xact lock serializes concurrent boots (web pod + import job).
SELECT pg_advisory_xact_lock(hashtext('warframe-tracker-migrate'));

-- Game data: written only by the importer.

CREATE TABLE IF NOT EXISTS wiki_sources (
  title text PRIMARY KEY,
  rev_id bigint NOT NULL,
  fetched_at timestamptz NOT NULL DEFAULT now(),
  source text NOT NULL
);

CREATE TABLE IF NOT EXISTS import_runs (
  id bigserial PRIMARY KEY,
  finished_at timestamptz NOT NULL DEFAULT now(),
  summary jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS items (
  id bigserial PRIMARY KEY,
  wiki_name text NOT NULL,
  category text NOT NULL CHECK (category IN
    ('warframe','weapon','mod','arcane','resource','component','relic')),
  subcategory text,
  is_prime boolean NOT NULL DEFAULT false,
  is_vaulted boolean,
  image text,
  wiki_link text,
  data jsonb NOT NULL DEFAULT '{}',
  removed_at timestamptz,
  UNIQUE (category, wiki_name)
);
CREATE INDEX IF NOT EXISTS items_name_lower ON items (lower(wiki_name));

CREATE TABLE IF NOT EXISTS recipes (
  result_item_id bigint NOT NULL REFERENCES items(id),
  ingredient_item_id bigint NOT NULL REFERENCES items(id),
  count integer NOT NULL CHECK (count > 0),
  PRIMARY KEY (result_item_id, ingredient_item_id)
);

CREATE TABLE IF NOT EXISTS recipe_meta (
  result_item_id bigint PRIMARY KEY REFERENCES items(id),
  credits integer,
  build_seconds integer
);

CREATE TABLE IF NOT EXISTS relic_drops (
  relic_item_id bigint NOT NULL REFERENCES items(id),
  part_item_id bigint NOT NULL REFERENCES items(id),
  rarity text,
  PRIMARY KEY (relic_item_id, part_item_id)
);

CREATE TABLE IF NOT EXISTS drop_sources (
  id bigserial PRIMARY KEY,
  source_kind text NOT NULL CHECK (source_kind IN ('mission','enemy','container','planet')),
  source_name text NOT NULL,
  rotation text,
  item_id bigint NOT NULL REFERENCES items(id),
  chance numeric,
  quantity integer
);
CREATE INDEX IF NOT EXISTS drop_sources_item ON drop_sources (item_id);
-- Conclave (PvP) reward tables; the planner hides them unless asked.
ALTER TABLE drop_sources ADD COLUMN IF NOT EXISTS is_pvp boolean NOT NULL DEFAULT false;

-- User data: every table keyed by user_id.

CREATE TABLE IF NOT EXISTS users (
  id bigserial PRIMARY KEY,
  name text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO users (id, name) VALUES (1, 'default') ON CONFLICT DO NOTHING;
SELECT setval('users_id_seq', GREATEST((SELECT max(id) FROM users), 1));

CREATE TABLE IF NOT EXISTS wishlist (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  added_at timestamptz NOT NULL DEFAULT now(),
  completed_at timestamptz,
  PRIMARY KEY (user_id, item_id)
);

CREATE TABLE IF NOT EXISTS component_progress (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  obtained boolean NOT NULL,
  PRIMARY KEY (user_id, item_id)
);

CREATE TABLE IF NOT EXISTS inventory (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  count bigint NOT NULL CHECK (count >= 0),
  updated_at timestamptz NOT NULL DEFAULT now(),
  source text NOT NULL DEFAULT 'manual' CHECK (source IN ('manual','plugin')),
  PRIMARY KEY (user_id, item_id)
);
