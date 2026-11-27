import type { DatabaseSync } from "node:sqlite";
// Once applied, these statements are immutable. Add a migration to change the schema.
const migrations = [
  `
  CREATE TABLE lane (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'paused', 'archived')),
    owner_agent_id TEXT REFERENCES agent(id),
    priority_rank INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
  ) STRICT;

  CREATE TABLE decision (
    id TEXT PRIMARY KEY,
    lane_id TEXT REFERENCES lane(id),
    authority TEXT NOT NULL CHECK (authority IN ('owner', 'chief_pm')),
    category TEXT NOT NULL CHECK (category IN ('spending', 'credentials', 'infra_network', 'release', 'direction', 'policy_exception', 'weekly_plan', 'routine')),
    urgency TEXT NOT NULL CHECK (urgency IN ('incident', 'blocking_today', 'digest')),
    title TEXT NOT NULL,
    evidence_json TEXT NOT NULL CHECK (json_valid(evidence_json)),
    recommendation TEXT NOT NULL,
    options_json TEXT NOT NULL CHECK (json_valid(options_json)),
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'superseded', 'expired')),
    requested_by TEXT NOT NULL,
    decided_by TEXT,
    answer_json TEXT NOT NULL CHECK (json_valid(answer_json)),
    rationale TEXT,
    superseded_by_id TEXT REFERENCES decision(id),
    created_at TEXT NOT NULL,
    decided_at TEXT,
    CHECK ((status = 'superseded') = (superseded_by_id IS NOT NULL)),
    CHECK (superseded_by_id IS NULL OR superseded_by_id <> id),
    CHECK ((decided_by IS NULL) = (decided_at IS NULL)),
    CHECK (status NOT IN ('approved', 'rejected') OR decided_by IS NOT NULL)
  ) STRICT, WITHOUT ROWID;

  CREATE TRIGGER decision_is_immutable BEFORE UPDATE ON decision
  WHEN NEW.id IS NOT OLD.id OR OLD.status IN ('superseded', 'expired')
    OR (OLD.status IN ('approved', 'rejected') AND NOT (
      NEW.status IS 'superseded' AND NEW.superseded_by_id IS NOT NULL
      AND NEW.lane_id IS OLD.lane_id
      AND NEW.authority IS OLD.authority
      AND NEW.category IS OLD.category
      AND NEW.urgency IS OLD.urgency
      AND NEW.title IS OLD.title
      AND NEW.evidence_json IS OLD.evidence_json
      AND NEW.recommendation IS OLD.recommendation
      AND NEW.options_json IS OLD.options_json
      AND NEW.requested_by IS OLD.requested_by
      AND NEW.decided_by IS OLD.decided_by
      AND NEW.answer_json IS OLD.answer_json
      AND NEW.rationale IS OLD.rationale
      AND NEW.created_at IS OLD.created_at
      AND NEW.decided_at IS OLD.decided_at
    ))
  BEGIN
    SELECT RAISE(ABORT, 'decision is immutable');
  END;

  CREATE TRIGGER decision_no_delete BEFORE DELETE ON decision
  BEGIN
    SELECT RAISE(ABORT, 'decisions cannot be deleted');
  END;

  CREATE TRIGGER decision_no_replace BEFORE INSERT ON decision
  WHEN EXISTS (SELECT 1 FROM decision WHERE id = NEW.id)
  BEGIN
    SELECT RAISE(ABORT, 'decision already exists');
  END;

  CREATE TABLE decision_event (
    id INTEGER PRIMARY KEY,
    decision_id TEXT NOT NULL REFERENCES decision(id),
    kind TEXT NOT NULL CHECK (kind IN ('created', 'answered', 'superseded', 'expired')),
    actor TEXT NOT NULL,
    detail_json TEXT NOT NULL CHECK (json_valid(detail_json)),
    created_at TEXT NOT NULL
  ) STRICT;
  CREATE INDEX decision_event_by_decision ON decision_event(decision_id, id);

  CREATE TRIGGER decision_event_no_update BEFORE UPDATE ON decision_event
  BEGIN
    SELECT RAISE(ABORT, 'decision events are append-only');
  END;

  CREATE TRIGGER decision_event_no_delete BEFORE DELETE ON decision_event
  BEGIN
    SELECT RAISE(ABORT, 'decision events are append-only');
  END;

  CREATE TRIGGER decision_event_no_replace BEFORE INSERT ON decision_event
  WHEN EXISTS (SELECT 1 FROM decision_event WHERE id = NEW.id)
  BEGIN
    SELECT RAISE(ABORT, 'decision events are append-only');
  END;

  CREATE TABLE goal (
    id TEXT PRIMARY KEY,
    lane_id TEXT NOT NULL REFERENCES lane(id),
    statement TEXT NOT NULL,
    metric_name TEXT NOT NULL,
    metric_unit TEXT,
    metric_current REAL,
    metric_target REAL NOT NULL,
    week_start TEXT NOT NULL,
    weekly_plan_json TEXT NOT NULL CHECK (json_valid(weekly_plan_json)),
    approved_decision_id TEXT REFERENCES decision(id),
    updated_at TEXT NOT NULL,
    UNIQUE (lane_id, week_start)
  ) STRICT;

  CREATE TABLE policy_rule (
    id TEXT PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    source_ref TEXT NOT NULL,
    mode TEXT NOT NULL DEFAULT 'observe' CHECK (mode IN ('observe', 'gate')),
    params_json TEXT NOT NULL CHECK (json_valid(params_json)),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    updated_at TEXT NOT NULL
  ) STRICT;

  INSERT INTO policy_rule (id, code, source_ref, params_json, enabled, updated_at)
  SELECT code, code, source_ref, '{}', 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
  FROM (
    SELECT 'two_failure_stop' AS code, 'team-policy.md §3' AS source_ref
    UNION ALL SELECT 'authority_routing', 'team-policy.md §1, §4'
    UNION ALL SELECT 'pr_green_ci', 'team-policy.md §3'
    UNION ALL SELECT 'host_limits', 'team-policy.md §3'
    UNION ALL SELECT 'registered_automation_only', 'team-policy.md §3'
  );

  CREATE TABLE policy_event (
    id TEXT PRIMARY KEY,
    rule_code TEXT NOT NULL REFERENCES policy_rule(code),
    lane_id TEXT REFERENCES lane(id),
    summary TEXT NOT NULL,
    detail_json TEXT NOT NULL CHECK (json_valid(detail_json)),
    created_at TEXT NOT NULL
  ) STRICT;

  CREATE TABLE friction_entry (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('correction', 'repeated_ask', 'owner_intervention', 'gate_block', 'policy_break')),
    summary TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open', 'selected', 'kept', 'reverted')),
    lane_id TEXT REFERENCES lane(id),
    rule_code TEXT REFERENCES policy_rule(code),
    created_at TEXT NOT NULL
  ) STRICT;
  CREATE INDEX friction_owner_by_time ON friction_entry(created_at) WHERE kind = 'owner_intervention';

  CREATE TABLE budget (
    id TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('team', 'lane', 'agent')),
    scope_id TEXT,
    period TEXT NOT NULL CHECK (period IN ('daily', 'weekly', 'monthly')),
    cap_usd REAL NOT NULL CHECK (cap_usd >= 0),
    warn_pct REAL NOT NULL CHECK (warn_pct BETWEEN 0 AND 100),
    spend_source TEXT NOT NULL CHECK (spend_source IN ('semon', 'magpie')),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    updated_at TEXT NOT NULL,
    CHECK ((scope_kind = 'team') = (scope_id IS NULL))
  ) STRICT;
  CREATE UNIQUE INDEX budget_by_scope_period ON budget(scope_kind, coalesce(scope_id, ''), period);

  CREATE TABLE spend_snapshot (
    session_uid TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK (source IN ('semon', 'magpie')),
    input_tokens INTEGER CHECK (input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    cached_tokens INTEGER CHECK (cached_tokens >= 0),
    usd_est REAL,
    source_reported_usd REAL,
    billed_usd REAL,
    coverage TEXT,
    snapshot_id TEXT,
    as_of TEXT,
    observed_at TEXT NOT NULL
  ) STRICT;

  CREATE TABLE agent (
    id TEXT PRIMARY KEY,
    role TEXT NOT NULL CHECK (role IN ('chief_pm', 'manager', 'worker', 'sentinel')),
    lane_id TEXT REFERENCES lane(id),
    connector_kind TEXT NOT NULL CHECK (connector_kind IN ('t3', 'semon', 'github', 'magpie', 'kiro', 'antigravity', 'codex')),
    host_id TEXT NOT NULL,
    external_ref TEXT NOT NULL,
    label TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'idle', 'waiting', 'offline', 'unknown')),
    last_seen_at TEXT,
    updated_at TEXT NOT NULL
  ) STRICT;

  CREATE TABLE run (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agent(id),
    lane_id TEXT REFERENCES lane(id),
    connector_kind TEXT NOT NULL CHECK (connector_kind IN ('t3', 'semon', 'github', 'magpie', 'kiro', 'antigravity', 'codex')),
    host_id TEXT NOT NULL,
    external_run_ref TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'waiting_input', 'waiting_approval', 'completed', 'failed', 'interrupted', 'cancelled', 'stalled')),
    model TEXT,
    started_at TEXT,
    completed_at TEXT,
    failure_count INTEGER NOT NULL CHECK (failure_count >= 0),
    waiting_detail_json TEXT NOT NULL CHECK (json_valid(waiting_detail_json)),
    semon_session_uid TEXT,
    updated_at TEXT NOT NULL
  ) STRICT;
  `,
];

// Read the version with the writer lock held so simultaneous opens cannot apply the same migration.
export function migrate(db: DatabaseSync): void {
  db.exec("BEGIN IMMEDIATE");
  try {
    db.exec("CREATE TABLE IF NOT EXISTS platform_schema_version (version INTEGER PRIMARY KEY) STRICT");
    const row = db.prepare("SELECT coalesce(max(version), 0) AS version FROM platform_schema_version").get();
    const current = Number(row?.["version"]);
    if (current > migrations.length) throw new Error(`unsupported platform schema version ${current}`);
    const record = db.prepare("INSERT INTO platform_schema_version (version) VALUES (?)");
    for (const [index, sql] of migrations.entries()) {
      const version = index + 1;
      if (version <= current) continue;
      db.exec(sql);
      record.run(version);
    }
    db.exec("COMMIT");
  } catch (error) {
    db.exec("ROLLBACK");
    throw error;
  }
}
