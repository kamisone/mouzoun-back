DO $$
DECLARE
    t record;
BEGIN
    FOR t IN
        SELECT tg.tgname, c.relname
        FROM pg_trigger tg
        JOIN pg_class c ON c.oid = tg.tgrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_proc p ON p.oid = tg.tgfoid
        WHERE n.nspname = current_schema()
          AND p.proname = 'set_updated_at'
          AND NOT tg.tgisinternal
    LOOP
        EXECUTE format('DROP TRIGGER %I ON %I', t.tgname, t.relname);
    END LOOP;
END;
$$;
