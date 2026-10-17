CREATE TABLE IF NOT EXISTS audit_events (
    service text NOT NULL,
    event_id text NOT NULL,
    payload jsonb NOT NULL,
    content_hash text NOT NULL,
    occurred_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (service, event_id)
);

CREATE INDEX IF NOT EXISTS audit_events_expiry ON audit_events (expires_at);

CREATE TABLE IF NOT EXISTS audit_delivery (
    service text NOT NULL,
    event_id text NOT NULL,
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    acknowledged_at timestamptz,
    PRIMARY KEY (service, event_id),
    FOREIGN KEY (service, event_id) REFERENCES audit_events (service, event_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS audit_delivery_pending ON audit_delivery (next_attempt_at)
    WHERE acknowledged_at IS NULL;

CREATE OR REPLACE FUNCTION audit_enqueue_event() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('INSERT INTO %I.audit_delivery (service, event_id) VALUES ($1, $2)', TG_TABLE_SCHEMA)
        USING NEW.service, NEW.event_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS audit_event_enqueue ON audit_events;
CREATE TRIGGER audit_event_enqueue AFTER INSERT ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_enqueue_event();

CREATE OR REPLACE FUNCTION audit_guard_retained_event() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.expires_at <= CURRENT_TIMESTAMP THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'retained audit events are immutable' USING ERRCODE = '23514';
END;
$$;

DROP TRIGGER IF EXISTS audit_event_immutable ON audit_events;
CREATE TRIGGER audit_event_immutable BEFORE UPDATE OR DELETE ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_guard_retained_event();

CREATE OR REPLACE FUNCTION audit_reject_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit events cannot be truncated' USING ERRCODE = '23514';
END;
$$;

DROP TRIGGER IF EXISTS audit_no_truncate ON audit_events;
CREATE TRIGGER audit_no_truncate BEFORE TRUNCATE ON audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION audit_reject_truncate();
