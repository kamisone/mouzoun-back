-- 0011_updated_at_triggers: attach set_updated_at() to every base table that has an
-- updated_at column at this point of the history.
--
-- RULE: this migration only covers the tables that exist when it runs. EVERY FUTURE TABLE
-- WITH AN updated_at COLUMN MUST CREATE ITS OWN TRIGGER in its own migration:
--
--     CREATE TRIGGER <table>_set_updated_at BEFORE UPDATE ON <table>
--         FOR EACH ROW EXECUTE FUNCTION set_updated_at();

DO $$
DECLARE
    t record;
BEGIN
    FOR t IN
        SELECT c.table_name
        FROM information_schema.columns c
        JOIN information_schema.tables tb
          ON tb.table_schema = c.table_schema AND tb.table_name = c.table_name
        WHERE c.table_schema = current_schema()
          AND c.column_name = 'updated_at'
          AND tb.table_type = 'BASE TABLE'
        ORDER BY c.table_name
    LOOP
        EXECUTE format(
            'CREATE TRIGGER %I BEFORE UPDATE ON %I FOR EACH ROW EXECUTE FUNCTION set_updated_at()',
            t.table_name || '_set_updated_at', t.table_name
        );
    END LOOP;
END;
$$;
