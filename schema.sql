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
-- Structured place (planet or open world) to group by; NULL for enemies (see enemy_locations).
ALTER TABLE drop_sources ADD COLUMN IF NOT EXISTS planet text;
CREATE INDEX IF NOT EXISTS drop_sources_planet ON drop_sources (planet);

-- Where an enemy appears, from Module:Enemies/data/<faction> (General.Planets × TileSets).
CREATE TABLE IF NOT EXISTS enemy_locations (
  enemy_name text NOT NULL,
  planet text NOT NULL,
  tileset text NOT NULL DEFAULT '',
  PRIMARY KEY (enemy_name, planet, tileset)
);
-- Resources: the wiki's prose "Location:" line. Display only.
ALTER TABLE items ADD COLUMN IF NOT EXISTS location_hint text;

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

-- Only the SHA-256 of a token is stored; the plaintext is printed once by `token create`.
CREATE TABLE IF NOT EXISTS api_tokens (
  id bigserial PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users(id),
  name text NOT NULL UNIQUE,
  token_hash bytea NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz,
  revoked_at timestamptz
);

-- Sync maps game paths to items by the wiki InternalName.
CREATE INDEX IF NOT EXISTS items_internal_name ON items ((data->>'InternalName'));

-- Discord sign-in. user 1 keeps discord_id NULL until the owner's first login claims it.
ALTER TABLE users ADD COLUMN IF NOT EXISTS discord_id text UNIQUE;
ALTER TABLE users ADD COLUMN IF NOT EXISTS display_name text;
ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar text;
ALTER TABLE users ADD COLUMN IF NOT EXISTS last_login_at timestamptz;

-- Only the SHA-256 of the session cookie is stored, as with api_tokens.
CREATE TABLE IF NOT EXISTS sessions (
  id bigserial PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash bytea NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NOT NULL
);

-- Token names are per user, among live tokens (a revoked name can be reused).
ALTER TABLE api_tokens DROP CONSTRAINT IF EXISTS api_tokens_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS api_tokens_user_name ON api_tokens (user_id, name)
  WHERE revoked_at IS NULL;

-- ON DELETE CASCADE for user-owned tables, so deleting a user deletes their data.
-- ponytail: drop-and-re-add on every boot; guard on pg_constraint.confdeltype if boot time matters.
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['wishlist','component_progress','inventory','api_tokens'] LOOP
    EXECUTE format('ALTER TABLE %I DROP CONSTRAINT IF EXISTS %I', t, t || '_user_id_fkey');
    EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE', t, t || '_user_id_fkey');
  END LOOP;
END $$;
